//! The ✦ Skills view (both front ends): every skill a session of this canvas
//! can get, by where it comes from (the canvas plugin, group skill folders,
//! the projects of its sessions, each agent's personal folders, the canvas
//! library), each with the agents that load it (`canvas_model::skills`; the
//! backend lists them, `Request::SkillSources`). The *Agent* row shows one
//! agent's skills at a time. Skills an agent's CLI manages (personal ones)
//! keep Space and Del; everything else is changed in its folder. Viewing
//! never starts a session. The skill folders editor is `skill_folders.rs`.

use canvas_model::skills::{self as sk, FolderSet, SkillSource, SourceKind, SourceSkill};

use crate::agents::AgentKind;
use crate::app::{App, AppEvent};
use crate::config::Kind;
use crate::dialogs::{Confirm, Modal};
use crate::paint::{ATTENTION, BLUE, DARK, POSITIVE, PURPLE};
use crate::side::{ItemRef, ListRow, RightDetail, SideView};

/// The view's state.
#[derive(Default)]
pub struct SkillsState {
    /// The backend's listing, or why there is none.
    pub sources: Option<Result<Vec<SkillSource>, String>>,
    pub loading: bool,
    /// Asked to list again while a listing ran.
    pub again: bool,
    /// Only the skills this agent loads (`None`: every agent).
    pub filter: Option<AgentKind>,
    /// A source to select once the listing is there (`skills::key`).
    pub focus: Option<String>,
}

/// What the view's rows, panels and menus ask for (`dialogs::Action::Skills`).
#[derive(Clone, Debug, PartialEq)]
pub enum SkillAction {
    /// Shows the view at a source (`skills::key`, for example `group:<id>`).
    Show(String),
    /// Opens a SKILL.md (VS Code; remotely through the tunnel or the viewer).
    Open(String),
    /// Opens a folder on the service's machine.
    Folder(String),
    /// The skill folders of a group (its id), else of every session.
    EditFolders(Option<String>),
    /// Turns a skill an agent's CLI manages on or off.
    Toggle(AgentKind, String, bool),
    /// Removes a skill an agent's CLI manages (asks first).
    Remove(AgentKind, String),
    /// The folder browser for the skill folders editor.
    Browse(String),
}

/// A skill row's identity within its source: its SKILL.md, else `#<name>`.
pub fn skill_id(skill: &SourceSkill) -> String {
    if skill.path.is_empty() { format!("#{}", skill.name) } else { skill.path.clone() }
}

/// The source a row of a group's skill folders belongs to (`group:` for every session's).
pub fn group_key(group: Option<&str>) -> String {
    sk::key(&SourceKind::Group { id: group.unwrap_or_default().to_owned(), name: String::new() })
}

impl App {
    /// What the backend needs: the canvas's working folders (session boxes,
    /// their history, group folders), its id and every skill folder set.
    pub fn skill_request(&self) -> (Vec<String>, Option<String>, Vec<FolderSet>) {
        let mut dirs: Vec<String> = Vec::new();
        let sessions = self.doc.nodes.iter().filter(|n| n.kind == crate::doc::NodeKind::Session && n.command.is_none() && !n.is_canvas_agent());
        for node in sessions {
            let cwd = node.cwd.clone().filter(|c| !c.trim().is_empty()).or_else(|| self.summary(node).map(|s| s.cwd.clone()).filter(|c| !c.trim().is_empty()));
            dirs.extend(cwd);
        }
        dirs.extend(self.doc.groups.iter().filter_map(|g| g.folder.clone()));
        let dirs: Vec<String> = canvas_model::doc::union_dirs(&dirs, &[]).into_iter().take(sk::MAX_DIRS).collect();
        let mut folders = vec![FolderSet { group: String::new(), name: String::new(), dirs: self.doc.defaults.skill_dirs.clone() }];
        for group in &self.doc.groups {
            if let Some(dirs) = group.defaults.as_ref().map(|d| d.skill_dirs.clone()).filter(|d| !d.is_empty()) {
                folders.push(FolderSet { group: group.id.clone(), name: group.name.clone(), dirs });
            }
        }
        let canvas = self.doc.canvas_id.clone().filter(|id| crate::doc::valid_canvas_id(id));
        (dirs, canvas, folders)
    }

    /// Lists the sources again (in the background) and loads the agents'
    /// own listings that are missing (`force`: all of them again).
    pub fn refresh_skills(&mut self, force: bool) {
        for kind in self.skill_agents() {
            if force || !self.agent_configs.contains_key(&kind) {
                self.refresh_config_for(kind);
            }
        }
        self.refresh_skill_sources();
    }

    /// Lists the sources again, in the background (once at a time; asked
    /// while one runs, another follows it).
    pub fn refresh_skill_sources(&mut self) {
        if self.skills.loading {
            self.skills.again = true;
            return;
        }
        self.skills.loading = true;
        let (dirs, canvas, folders) = self.skill_request();
        self.in_background(
            move |backend| {
                backend.skill_sources(dirs, canvas, folders).map_err(|e| {
                    let text = format!("{e:#}");
                    if text.contains("does not support") { "the service is older than this view: update it (personal skills only)".to_owned() } else { text }
                })
            },
            |result| AppEvent::Run(Box::new(move |app: &mut App| app.on_skill_sources(result))),
        );
    }

    /// The listing arrived.
    pub fn on_skill_sources(&mut self, result: Result<Vec<SkillSource>, String>) {
        self.skills.loading = false;
        self.skills.sources = Some(result);
        if std::mem::take(&mut self.skills.again) {
            self.refresh_skill_sources();
        }
        self.apply_skill_focus();
        // A source this listing does not have is forgotten (no jump on a later one).
        if !self.skills.loading {
            self.skills.focus = None;
        }
        self.needs_draw = true;
    }

    /// Selects the row of the source the view was opened at, once it is listed.
    fn apply_skill_focus(&mut self) {
        if self.side_view != SideView::Skills {
            return;
        }
        let Some(key) = self.skills.focus.clone() else { return };
        let rows = self.skill_rows();
        if let Some(at) = rows.iter().position(|r| r.item == ItemRef::SkillSource(key.clone()) || r.item == ItemRef::SkillFolders(key.strip_prefix("group:").map(|g| g.to_owned()).filter(|g| !g.is_empty()))) {
            self.list_sel = at;
            if matches!(rows[at].item, ItemRef::SkillSource(_)) {
                self.right_detail = Some(RightDetail::Item(rows[at].item.clone()));
            }
            self.skills.focus = None;
        }
    }

    /// Agents the view knows: the installed ones (every agent when none is known).
    pub fn skill_agents(&self) -> Vec<AgentKind> {
        if self.installed_agents.is_empty() { crate::agents::ALL.to_vec() } else { crate::agents::ALL.into_iter().filter(|k| self.installed_agents.contains(k)).collect() }
    }

    /// Every source with the agents' own listings merged in.
    pub fn skill_sources(&self) -> Vec<SkillSource> {
        let mut sources = match &self.skills.sources {
            Some(Ok(sources)) => sources.clone(),
            _ => Vec::new(),
        };
        for kind in self.skill_agents() {
            if let Some(config) = self.agent_configs.get(&kind) {
                sk::merge_listing(&mut sources, kind, &config.skills);
            }
        }
        sources
    }

    /// The sources as the view shows them (the agent filter applied).
    pub fn visible_skill_sources(&self) -> Vec<SkillSource> {
        sk::for_agent(&self.skill_sources(), self.skills.filter)
    }

    /// A row's skill: the shown sources, its source's index and its own.
    pub fn find_skill(&self, key: &str, id: &str) -> Option<(Vec<SkillSource>, usize, usize)> {
        let sources = self.visible_skill_sources();
        let source = sources.iter().position(|s| sk::key(&s.kind) == key)?;
        let skill = sources[source].skills.iter().position(|s| skill_id(s) == id)?;
        Some((sources, source, skill))
    }

    /// The *Agent* row: every agent, then each installed agent alone.
    pub fn cycle_skill_filter(&mut self) {
        let agents = self.skill_agents();
        self.skills.filter = match self.skills.filter {
            None => agents.first().copied(),
            Some(kind) => agents.iter().position(|k| *k == kind).and_then(|i| agents.get(i + 1)).copied(),
        };
        self.right_detail = None;
        self.needs_draw = true;
    }

    /// Opens the view at a source (a group's details, its menu).
    pub fn open_skills_at(&mut self, key: &str) {
        self.skills.focus = Some(key.to_owned());
        self.set_view(SideView::Skills);
        self.apply_skill_focus();
    }
}

/// A skill's row: its state (a CLI-managed one's on/off), its name and the
/// agents that load it (`≈`: an agent that loads it also loads another skill of its name).
fn skill_row(sources: &[SkillSource], source: usize, skill: usize, right: String) -> ListRow {
    let entry = &sources[source].skills[skill];
    let (icon, color) = match entry.enabled {
        Some(true) => ("●", POSITIVE),
        Some(false) => ("○", DARK),
        None if !entry.problem.is_empty() => ("✦", ATTENTION),
        None => ("✦", PURPLE),
    };
    let right = if sk::same_name(sources, source, skill).is_empty() { right } else { format!("≈ {right}") };
    ListRow::row(ItemRef::SkillOf(sk::key(&sources[source].kind), skill_id(entry)), icon, color, entry.name.clone(), right)
}

/// "2 folders · 5 skills".
pub fn folders_summary(folders: usize, skills: Option<usize>) -> String {
    let folders = format!("{folders} folder{}", if folders == 1 { "" } else { "s" });
    match skills {
        Some(n) => format!("{folders} · {n} skill{}", if n == 1 { "" } else { "s" }),
        None => folders,
    }
}

impl App {
    /// The view's rows: CANVAS, GROUP SKILL FOLDERS, PROJECTS, PERSONAL, CANVAS LIBRARY.
    pub fn skill_rows(&self) -> Vec<ListRow> {
        let label = self.skills.filter.map(|k| k.label()).unwrap_or("All agents");
        let mut rows = vec![ListRow::row(ItemRef::SkillFilter, "⇄", BLUE, format!("Agent: {label}"), "A")];
        match &self.skills.sources {
            None => rows.push(ListRow::info("Loading the skill sources…")),
            Some(Err(why)) => rows.push(ListRow::info(format!("⚠ {why}"))),
            Some(Ok(_)) => {}
        }
        let sources = self.visible_skill_sources();
        let tags = |s: &SourceSkill| sk::tags(&s.agents);
        let of = |kind: fn(&SourceKind) -> bool| sources.iter().enumerate().filter(move |(_, s)| kind(&s.kind)).map(|(i, _)| i).collect::<Vec<_>>();
        rows.push(ListRow::header("CANVAS"));
        for si in of(|k| *k == SourceKind::Canvas) {
            for (ki, skill) in sources[si].skills.iter().enumerate() {
                rows.push(skill_row(&sources, si, ki, tags(skill)));
            }
        }
        rows.push(ListRow::header("GROUP SKILL FOLDERS"));
        let mut scopes: Vec<(Option<String>, String, usize)> = vec![(None, "All sessions".into(), self.doc.defaults.skill_dirs.len())];
        scopes.extend(self.doc.groups.iter().filter_map(|g| g.defaults.as_ref().filter(|d| !d.skill_dirs.is_empty()).map(|d| (Some(g.id.clone()), g.name.clone(), d.skill_dirs.len()))));
        for (group, name, count) in &scopes {
            let key = group_key(group.as_deref());
            if *count == 0 {
                rows.push(ListRow::row(ItemRef::SkillFolders(None), "◧", BLUE, name.clone(), "none · add…"));
                continue;
            }
            let at = sources.iter().position(|s| sk::key(&s.kind) == key);
            let found = at.map(|i| sources[i].skills.len());
            rows.push(ListRow::row(ItemRef::SkillSource(key), "◧", BLUE, name.clone(), folders_summary(*count, found)));
            if let Some(si) = at {
                for (ki, skill) in sources[si].skills.iter().enumerate() {
                    rows.push(skill_row(&sources, si, ki, tags(skill)));
                }
            }
        }
        if scopes.len() == 1 {
            rows.push(ListRow::info("Group menu › Skill folders…"));
        }
        rows.push(ListRow::header("PROJECTS"));
        let projects = of(|k| matches!(k, SourceKind::Project { .. }));
        for &si in projects.iter().filter(|&&si| !sources[si].skills.is_empty()) {
            rows.push(ListRow::row(ItemRef::SkillSource(sk::key(&sources[si].kind)), "▸", BLUE, sk::title(&sources[si].kind), sources[si].path.clone()));
            for (ki, skill) in sources[si].skills.iter().enumerate() {
                rows.push(skill_row(&sources, si, ki, tags(skill)));
            }
        }
        let bare = projects.iter().filter(|&&si| sources[si].skills.is_empty()).count();
        if bare > 0 {
            rows.push(ListRow::info(format!("{bare} folder{} without skills", if bare == 1 { "" } else { "s" })));
        } else if projects.is_empty() && matches!(self.skills.sources, Some(Ok(_))) {
            rows.push(ListRow::info("No session folders yet."));
        }
        rows.push(ListRow::header("PERSONAL"));
        for si in of(|k| matches!(k, SourceKind::Personal { .. })) {
            let count = sources[si].skills.len();
            rows.push(ListRow::row(ItemRef::SkillSource(sk::key(&sources[si].kind)), "▸", BLUE, sk::title(&sources[si].kind), format!("{count} skill{}", if count == 1 { "" } else { "s" })));
            for (ki, skill) in sources[si].skills.iter().enumerate() {
                rows.push(skill_row(&sources, si, ki, tags(skill)));
            }
        }
        rows.push(ListRow::header("CANVAS LIBRARY"));
        let library = of(|k| *k == SourceKind::Library);
        let agents = self.canvas_agents();
        for &si in &library {
            for (ki, skill) in sources[si].skills.iter().enumerate() {
                let given: Vec<&str> = agents.iter().filter(|n| n.passive.as_ref().is_some_and(|p| p.skills.contains(&skill.name))).map(|n| n.name.as_str()).collect();
                rows.push(skill_row(&sources, si, ki, if given.is_empty() { "not given".into() } else { given.join(", ") }));
            }
        }
        if library.iter().all(|&si| sources[si].skills.is_empty()) {
            rows.push(ListRow::info(match self.canvas_agents_unavailable() {
                Some(why) => format!("Unavailable: {why}"),
                None => "None: ◎ Agents › New skill…".to_owned(),
            }));
        }
        rows.push(ListRow::row(ItemRef::Add(crate::side::AddKind::Skill), "+", BLUE, "Add skill (Copilot CLI)…", ""));
        rows.push(ListRow::row(ItemRef::Refresh, "⟳", BLUE, "Refresh", ""));
        for kind in self.skill_agents().into_iter().filter(|k| self.skills.filter.is_none_or(|f| f == *k)) {
            for error in self.agent_configs.get(&kind).map(|c| c.errors.clone()).unwrap_or_default() {
                rows.push(ListRow::info(format!("⚠ {}: {error}", kind.label())));
            }
        }
        rows
    }
}

/// Where a skill comes from, for messages ("PROJECTS › app").
pub fn source_label(kind: &SourceKind) -> String {
    let section = match kind {
        SourceKind::Canvas => "CANVAS",
        SourceKind::Library => "CANVAS LIBRARY",
        SourceKind::Group { .. } => "GROUP SKILL FOLDERS",
        SourceKind::Project { .. } => "PROJECTS",
        SourceKind::Personal { .. } => "PERSONAL",
    };
    format!("{section} › {}", sk::title(kind))
}

impl App {
    /// A click on a row of the view: a skill not selected yet is only
    /// selected (its details show); a click on the selected one opens it, as
    /// Enter does. `false`: not such a click.
    pub fn skills_click(&mut self, index: usize) -> bool {
        if self.side_view != SideView::Skills || index == self.list_sel {
            return false;
        }
        let rows = self.build_list();
        let Some(row) = rows.get(index).filter(|r| matches!(r.item, ItemRef::SkillOf(..))) else { return false };
        self.list_sel = index;
        self.right_detail = Some(RightDetail::Item(row.item.clone()));
        self.needs_draw = true;
        true
    }

    /// Enter on a row of the view; `false`: not one of its rows.
    pub fn skills_activate(&mut self, item: &ItemRef) -> bool {
        match item {
            ItemRef::SkillFilter => self.cycle_skill_filter(),
            ItemRef::SkillFolders(group) => self.run_skills(SkillAction::EditFolders(group.clone())),
            ItemRef::SkillSource(key) => match key.strip_prefix("group:") {
                Some(group) => self.run_skills(SkillAction::EditFolders(Some(group.to_owned()).filter(|g| !g.is_empty()))),
                None => self.right_detail = Some(RightDetail::Item(item.clone())),
            },
            ItemRef::SkillOf(key, id) => {
                self.right_detail = Some(RightDetail::Item(item.clone()));
                match self.find_skill(key, id) {
                    Some((sources, si, ki)) if !sources[si].skills[ki].path.is_empty() => self.run_skills(SkillAction::Open(sources[si].skills[ki].path.clone())),
                    Some(_) => self.notify("Its agent's CLI lists it without a file to open.", false),
                    None => self.notify("This skill is no longer listed; ⟳ Refresh.", true),
                }
            }
            _ => return false,
        }
        self.needs_draw = true;
        true
    }

    /// The agent whose CLI manages a row's skill, with the skill: `Err` says why there is none.
    fn managed_skill(&self, key: &str, id: &str) -> Result<(AgentKind, SourceSkill), String> {
        let Some((sources, si, ki)) = self.find_skill(key, id) else { return Err("This skill is no longer listed; ⟳ Refresh.".into()) };
        let skill = sources[si].skills[ki].clone();
        match (&sources[si].kind, skill.enabled) {
            (SourceKind::Personal { agent }, Some(_)) => Ok((*agent, skill)),
            (kind, _) => Err(format!("“{}” is not managed here: it comes from {}; change it in its folder (Enter opens its SKILL.md).", skill.name, source_label(kind))),
        }
    }

    /// Space on a skill row: on or off, through its agent's CLI.
    pub fn skills_toggle(&mut self, key: &str, id: &str) {
        match self.managed_skill(key, id) {
            Ok((agent, skill)) => self.run_skills(SkillAction::Toggle(agent, skill.name, !skill.enabled.unwrap_or(true))),
            Err(why) => self.notify(why, true),
        }
    }

    /// Del on a skill row: removed by its agent's CLI, after asking.
    pub fn skills_delete(&mut self, key: &str, id: &str) {
        let (agent, skill) = match self.managed_skill(key, id) {
            Ok(found) => found,
            Err(why) => return self.notify(why, true),
        };
        if !crate::config::caps_for_changes(agent, Kind::Skill).1 {
            return self.notify(format!("Not supported for {}.", agent.label()), true);
        }
        let source = skill.managed.to_lowercase();
        if source.contains("plugin") || source.contains("project") || source.contains("builtin") || source.contains("built-in") {
            return self.notify(format!("“{}” comes from {source}; disable it instead (Space).", skill.name), true);
        }
        self.modals.push(Modal::Confirm(Confirm {
            title: format!("Remove skill “{}”?", skill.name),
            body: vec![format!("{} performs the removal; this cannot be undone here.", agent.label())],
            buttons: vec![("Remove".into(), crate::dialogs::Action::Skills(SkillAction::Remove(agent, skill.name))), ("Cancel".into(), crate::dialogs::Action::Nothing)],
            sel: 1,
        }));
    }

    /// Runs a change through an agent's CLI, then reloads its listing and the sources.
    pub fn agent_skill_op(&mut self, agent: AgentKind, label: String, op: impl FnOnce(&dyn crate::rpc::Backend) -> anyhow::Result<String> + Send + 'static) {
        self.cli_busy = Some(label.clone());
        self.notify(format!("{label}…"), false);
        self.in_background(
            move |backend| op(backend).map_err(|e| format!("{e:#}")),
            move |result| {
                AppEvent::Run(Box::new(move |app: &mut App| {
                    app.cli_busy = None;
                    match result {
                        Ok(_) => app.notify(format!("{label}: done."), false),
                        Err(error) => app.notify(format!("{label}: {error}"), true),
                    }
                    app.configs_loading.remove(&agent);
                    app.refresh_config_for(agent);
                    if app.side_view == SideView::Skills {
                        app.refresh_skills(false);
                    }
                }))
            },
        );
    }

    pub fn run_skills(&mut self, action: SkillAction) {
        self.needs_draw = true;
        match action {
            SkillAction::Show(key) => self.open_skills_at(&key),
            SkillAction::Open(path) => self.open_service_file(&path),
            SkillAction::Folder(path) => self.open_folder(&path),
            SkillAction::EditFolders(group) => crate::skill_folders::open(self, group),
            SkillAction::Browse(start) => self.open_folder_browser(&start),
            SkillAction::Toggle(agent, name, enabled) => {
                if !crate::config::caps_for_changes(agent, Kind::Skill).0 {
                    return self.notify(format!("Not supported for {}.", agent.label()), true);
                }
                let label = format!("{} skill {name}", if enabled { "Enabling" } else { "Disabling" });
                self.agent_skill_op(agent, label, move |backend| backend.config_set_enabled(agent, Kind::Skill, &name, enabled));
            }
            SkillAction::Remove(agent, name) => self.agent_skill_op(agent, format!("Removing {name}"), move |backend| backend.config_remove(agent, Kind::Skill, &name)),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use canvas_model::rpc::{Backend, PathInfo, Request, Response};

    use super::*;
    use crate::agents::ALL;
    use crate::doc::{Doc, Group, LaunchDefaults, Node};

    /// A backend with the listing of [`sources`], two agents' own lists,
    /// folders under `C:\skills` (`empty` ones hold none); it records requests.
    #[derive(Default)]
    pub(crate) struct Skills {
        pub requests: Mutex<Vec<Request>>,
        /// When set, path checks wait for a message on it (a slow service).
        pub gate: Mutex<Option<crossbeam_channel::Receiver<()>>>,
    }

    fn skill(name: &str, agents: &[AgentKind], path: &str) -> SourceSkill {
        SourceSkill { name: name.into(), description: format!("{name} does things"), path: path.into(), agents: agents.to_vec(), folder: path.rsplitn(3, '\\').nth(2).unwrap_or_default().into(), ..Default::default() }
    }

    fn source(kind: SourceKind, path: &str, skills: Vec<SourceSkill>) -> SkillSource {
        SkillSource { agents: sk::agents_of(&skills), kind, path: path.into(), folders: vec![(path.into(), skills.len())], skills, problem: String::new() }
    }

    /// The listing: the canvas plugin, the folder sets asked for, a project,
    /// Copilot's and Claude Code's personal folders, the library.
    pub(crate) fn sources(folders: &[FolderSet]) -> Vec<SkillSource> {
        let (cp, oc, cc) = (AgentKind::Copilot, AgentKind::OpenCode, AgentKind::Claude);
        let mut out = vec![source(SourceKind::Canvas, r"C:\home\plugin\skills", vec![skill("canvas-events", &ALL, r"C:\home\plugin\skills\canvas-events\SKILL.md")])];
        for set in folders.iter().filter(|s| !s.dirs.is_empty()) {
            let dir = &set.dirs[0];
            let skills = if dir.contains("empty") { Vec::new() } else { vec![skill("team-notes", &ALL, &format!(r"{dir}\team-notes\SKILL.md")), skill("review", &ALL, &format!(r"{dir}\review\SKILL.md"))] };
            out.push(source(SourceKind::Group { id: set.group.clone(), name: set.name.clone() }, dir, skills));
        }
        out.push(source(SourceKind::Project { root: r"C:\repos\app".into() }, r"C:\repos\app", vec![skill("review", &[cp, oc, cc], r"C:\repos\app\.claude\skills\review\SKILL.md"), skill("ci", &[cp], r"C:\repos\app\.github\skills\ci\SKILL.md")]));
        out.push(source(SourceKind::Project { root: r"C:\notes".into() }, r"C:\notes", Vec::new()));
        out.push(source(SourceKind::Personal { agent: cp }, r"C:\u\.copilot\skills", vec![skill("mine", &[cp], r"C:\u\.copilot\skills\mine\SKILL.md")]));
        out.push(source(SourceKind::Personal { agent: cc }, r"C:\u\.claude\skills", vec![skill("tidy", &[oc, cc], r"C:\u\.claude\skills\tidy\SKILL.md")]));
        out.push(source(SourceKind::Library, r"C:\home\canvas-agents\c1\skills", vec![skill("pr-etiquette", &ALL, r"C:\home\canvas-agents\c1\skills\pr-etiquette\SKILL.md")]));
        out
    }

    impl Backend for Skills {
        fn call(&self, request: Request) -> Result<Response> {
            self.requests.lock().unwrap().push(request.clone());
            Ok(match request {
                Request::SkillSources { folders, .. } => Response::SkillSources(sources(&folders)),
                Request::PathInfo { path } => {
                    let gate = self.gate.lock().unwrap().clone();
                    if let Some(gate) = gate {
                        let _ = gate.recv_timeout(std::time::Duration::from_secs(5));
                    }
                    let dir = path.to_lowercase().starts_with(r"c:\skills");
                    Response::Path(PathInfo { exists: dir, dir, file: false, absolute: path, occupied: dir })
                }
                Request::ListDir { .. } => Response::Texts(vec!["..".into(), "team".into()]),
                Request::ConfigSnapshot { agent: AgentKind::Copilot } => {
                    let listed = |name: &str, path: &str, source: &str, enabled: bool| crate::config::Skill { name: name.into(), path: path.into(), source: source.into(), enabled, ..Default::default() };
                    let skills = vec![listed("mine", r"C:\u\.copilot\skills\mine", "personal", true), listed("from-plugin", "", "plugin", false)];
                    Response::Config(Box::new(crate::config::Snapshot { skills, ..Default::default() }))
                }
                Request::ConfigSnapshot { .. } => Response::Config(Box::default()),
                Request::ConfigSetEnabled { .. } | Request::ConfigRemove { .. } => Response::Text("ok".into()),
                _ => Response::Error("not here".into()),
            })
        }
    }

    pub(crate) fn node(id: &str, cwd: &str, x: i32) -> Node {
        let mut node: Node = serde_json::from_value(serde_json::json!({ "id": id, "kind": "session", "name": id, "x": x, "y": 2 })).unwrap();
        node.cwd = Some(cwd.into());
        node
    }

    /// A canvas with a session in a repository, a group "Payments" (folder
    /// `C:\notes`) with its own skill folder, and a canvas-wide agent.
    pub(crate) fn doc() -> Doc {
        let mut doc = Doc { canvas_id: Some("c1".into()), ..Default::default() };
        doc.nodes.push(node("s1", r"C:\repos\app", 2));
        doc.nodes.push(node("s2", r"c:\repos\APP\", 30));
        let mut wide = node("w", r"C:\elsewhere", 60);
        wide.passive = Some(crate::doc::Passive { key: "kw".into(), scope: crate::doc::PassiveScope::Canvas, skills: vec!["pr-etiquette".into()], ..Default::default() });
        wide.name = "PR coordinator".into();
        doc.nodes.push(wide);
        let defaults = LaunchDefaults { skill_dirs: vec![r"C:\skills\pay".into()], ..Default::default() };
        doc.groups.push(Group { id: "g1".into(), name: "Payments".into(), x: 0, y: 0, w: 60, h: 12, folder: Some(r"C:\notes".into()), defaults: Some(defaults), ..Default::default() });
        doc
    }

    pub(crate) struct Rig {
        pub app: App,
        pub events: crossbeam_channel::Receiver<AppEvent>,
        pub backend: Arc<Skills>,
        pub opened: Arc<Opened>,
    }

    /// What the front end was asked to open (nothing really opens in tests).
    #[derive(Default)]
    pub(crate) struct Opened(pub Mutex<Vec<String>>);

    impl crate::platform::Platform for Opened {
        fn open_link(&self, target: &str) -> Result<String> {
            self.0.lock().unwrap().push(target.to_owned());
            Ok("opened".into())
        }
    }

    pub(crate) fn rig() -> Rig {
        let (tx, _sent) = crossbeam_channel::bounded(64);
        let (_keep, service_rx) = crossbeam_channel::bounded(1);
        let (app_tx, events) = crossbeam_channel::unbounded();
        let backend = Arc::new(Skills::default());
        let mut app = App::new(crate::conn::Conn::detached(tx, service_rx), backend.clone(), crate::sync::Opened::detached(doc()), false, ".".into(), app_tx, None);
        app.installed_agents = vec![AgentKind::Copilot, AgentKind::Claude];
        let opened = Arc::new(Opened::default());
        app.platform = opened.clone();
        Rig { app, events, backend, opened }
    }

    /// Runs background continuations until `done`.
    pub(crate) fn settle(rig: &mut Rig, done: impl Fn(&App) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done(&rig.app) {
            assert!(std::time::Instant::now() < deadline, "background work did not finish");
            if let Ok(event) = rig.events.recv_timeout(std::time::Duration::from_millis(50)) {
                rig.app.on_app_event(event);
            }
        }
    }

    fn labels(app: &App) -> Vec<String> {
        app.build_list().iter().map(|r| if r.right.is_empty() { r.label.clone() } else { format!("{} | {}", r.label, r.right) }).collect()
    }

    #[test]
    fn opening_the_view_lists_every_source_with_the_agents_that_load_each_skill() {
        let mut r = rig();
        r.app.doc.defaults.skill_dirs = vec![r"C:\skills\all".into()];
        r.app.set_view(SideView::Skills);
        settle(&mut r, |app| matches!(app.skills.sources, Some(Ok(_))) && app.configs_loading.is_empty());
        // What the backend was asked: the sessions' folders (once), the group's folder, every folder set.
        let asked = r.backend.requests.lock().unwrap().iter().find_map(|q| match q {
            Request::SkillSources { dirs, canvas, folders } => Some((dirs.clone(), canvas.clone(), folders.clone())),
            _ => None,
        });
        let (dirs, canvas, folders) = asked.expect("a listing");
        assert_eq!((dirs, canvas.as_deref()), (vec![r"C:\repos\app".to_owned(), r"C:\notes".to_owned()], Some("c1")), "the canvas-wide agent's folder is not a project");
        assert_eq!(folders.iter().map(|f| (f.group.as_str(), f.dirs.clone())).collect::<Vec<_>>(), [("", vec![r"C:\skills\all".to_owned()]), ("g1", vec![r"C:\skills\pay".to_owned()])]);
        let rows = labels(&r.app);
        let expected = [
            "Agent: All agents | A",
            "CANVAS",
            "canvas-events | all",
            "GROUP SKILL FOLDERS",
            "All sessions | 1 folder · 2 skills",
            "team-notes | ≈ all",
            "review | ≈ all",
            "Payments | 1 folder · 2 skills",
            "team-notes | ≈ all",
            "review | ≈ all",
            "PROJECTS",
            r"app | C:\repos\app",
            "review | ≈ all",
            "ci | CP",
            "1 folder without skills",
            "PERSONAL",
            "GitHub Copilot | 2 skills",
            "mine | CP",
            "from-plugin | CP",
            "Claude Code | 1 skill",
            "tidy | OC CC",
            "CANVAS LIBRARY",
            "pr-etiquette | PR coordinator",
            "Add skill (Copilot CLI)…",
            "Refresh",
        ];
        assert_eq!(rows, expected);
        // Copilot's own list: its folder's skill is on, its plugin's skill off.
        let row = |label: &str| r.app.build_list().into_iter().find(|row| row.label == label).unwrap();
        assert_eq!((row("mine").icon, row("from-plugin").icon), ("●", "○"));
        // One agent at a time: only what it loads, only its own personal source.
        r.app.cycle_skill_filter();
        let copilot = labels(&r.app);
        assert_eq!(copilot[0], "Agent: GitHub Copilot | A");
        assert!(copilot.contains(&"ci | CP".to_owned()) && !copilot.iter().any(|l| l.starts_with("tidy") || l.starts_with("Claude Code")));
        r.app.cycle_skill_filter();
        assert!(labels(&r.app).iter().any(|l| l == "tidy | OC CC") && !labels(&r.app).iter().any(|l| l.starts_with("ci ")));
        r.app.cycle_skill_filter();
        assert_eq!(r.app.skills.filter, None, "back to every agent");
        assert!(!r.backend.requests.lock().unwrap().iter().any(|q| matches!(q, Request::CliCommand { .. })), "viewing starts nothing");
    }

    #[test]
    fn only_skills_an_agents_cli_manages_switch_and_the_others_say_where_they_live() {
        let mut r = rig();
        r.app.set_view(SideView::Skills);
        settle(&mut r, |app| matches!(app.skills.sources, Some(Ok(_))) && app.configs_loading.is_empty());
        let at = |app: &App, label: &str| app.build_list().iter().position(|row| row.label == label).unwrap();
        let index = at(&r.app, "ci");
        r.app.list_toggle(index);
        assert!(r.app.notice.as_ref().is_some_and(|n| n.text.contains("not managed here") && n.text.contains(r"PROJECTS › app")), "{:?}", r.app.notice.as_ref().map(|n| &n.text));
        let index = at(&r.app, "from-plugin");
        r.app.list_delete(index);
        assert!(r.app.notice.as_ref().is_some_and(|n| n.text.contains("comes from plugin")));
        let index = at(&r.app, "mine");
        r.app.list_delete(index);
        assert!(matches!(r.app.modals.last(), Some(Modal::Confirm(c)) if c.title == "Remove skill “mine”?"));
        r.app.modals.clear();
        r.app.list_toggle(index);
        settle(&mut r, |app| app.cli_busy.is_none());
        let asked = r.backend.requests.lock().unwrap().iter().any(|q| matches!(q, Request::ConfigSetEnabled { agent: AgentKind::Copilot, what: Kind::Skill, name, enabled: false } if name == "mine"));
        assert!(asked, "through Copilot's CLI");
        // Enter on a skill opens its SKILL.md and shows its details.
        let index = at(&r.app, "tidy");
        r.app.list_activate(index);
        assert!(matches!(&r.app.right_detail, Some(RightDetail::Item(ItemRef::SkillOf(key, id))) if key == "personal:claude" && id.ends_with(r"tidy\SKILL.md")));
        assert_eq!(*r.opened.0.lock().unwrap(), [r"C:\u\.claude\skills\tidy\SKILL.md"]);
    }

    #[test]
    fn a_groups_details_open_the_view_at_its_folders() {
        let mut r = rig();
        r.app.run(crate::dialogs::Action::Skills(SkillAction::Show(group_key(Some("g1")))));
        assert_eq!(r.app.side_view, SideView::Skills);
        settle(&mut r, |app| app.skills.focus.is_none());
        let rows = r.app.build_list();
        assert_eq!(rows[r.app.list_sel].item, ItemRef::SkillSource("group:g1".into()));
        assert_eq!(r.app.right_detail, Some(RightDetail::Item(ItemRef::SkillSource("group:g1".into()))));
        assert_eq!((group_key(None), folders_summary(2, Some(1)), folders_summary(1, None)), ("group:".into(), "2 folders · 1 skill".into(), "1 folder".into()));
    }

    #[test]
    fn a_click_shows_a_skill_and_only_a_second_click_opens_it() {
        use crate::input::{MouseButton, MouseEvent, MouseEventKind};
        let mut r = rig();
        r.app.set_view(SideView::Skills);
        settle(&mut r, |app| matches!(app.skills.sources, Some(Ok(_))) && app.configs_loading.is_empty());
        let index = r.app.build_list().iter().position(|row| row.label == "ci").unwrap();
        let area = ratatui::layout::Rect::new(0, 0, 160, 45);
        crate::ui::render(&mut r.app, &mut ratatui::buffer::Buffer::empty(area), area, None);
        let y = r.app.layout.sidebar_rows.iter().find(|(_, i)| *i == index).map(|(y, _)| *y).expect("the row on screen");
        let click = |app: &mut App| {
            for kind in [MouseEventKind::Down(MouseButton::Left), MouseEventKind::Up(MouseButton::Left)] {
                app.on_mouse(MouseEvent::at_cell(kind, 10, y, crate::input::Modifiers::NONE));
            }
        };
        click(&mut r.app);
        assert_eq!(r.app.list_sel, index);
        assert!(matches!(&r.app.right_detail, Some(RightDetail::Item(ItemRef::SkillOf(key, _))) if key == r"project:c:\repos\app"));
        assert!(r.opened.0.lock().unwrap().is_empty(), "a click only shows it");
        click(&mut r.app);
        assert_eq!(*r.opened.0.lock().unwrap(), [r"C:\repos\app\.github\skills\ci\SKILL.md"]);
    }

    #[test]
    fn a_source_the_listing_does_not_have_is_forgotten() {
        let mut r = rig();
        r.app.run(crate::dialogs::Action::Skills(SkillAction::Show("group:gone".into())));
        settle(&mut r, |app| !app.skills.loading);
        assert_eq!(r.app.skills.focus, None, "no jump when a later listing has it");
    }

    #[test]
    fn group_passive_runs_are_routed_with_the_canvas_and_group_folders() {
        let mut r = rig();
        r.app.doc.defaults.skill_dirs = vec![r"C:\skills\all".into()];
        let mut passive = node("p1", r"C:\notes", 10);
        passive.passive = Some(crate::doc::Passive { key: "kp".into(), group: "g1".into(), slug: "watch".into(), ..Default::default() });
        r.app.doc.nodes.push(passive);
        let routing = r.app.build_routing();
        let spec = |key: &str| routing.passive.iter().find(|p| p.key == key).cloned().unwrap();
        assert_eq!(spec("kp").skill_dirs, [r"C:\skills\all", r"C:\skills\pay"]);
        assert!(spec("kw").skill_dirs.is_empty(), "canvas-wide agents have their own skills");
    }
}
