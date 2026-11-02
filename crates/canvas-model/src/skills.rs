//! Where the skills of a canvas's sessions come from and which agents load
//! them: the folders each agent CLI reads by itself (from their official
//! docs; the table and its sources are in docs/AGENT-PROVIDERS.md), the projects of
//! the canvas's sessions, the canvas plugin, group skill folders, personal
//! folders and the canvas library. The backend lists them
//! (`Request::SkillSources`); before a launch the service copies a group's
//! skill folders into a plugin its agent CLI loads ([`prepare_group_plugin`]).
//! Skill folders are only read here; writes stay under the service's home.

use std::collections::{BTreeSet, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agent::{ALL, AgentKind};

/// Project roots one listing scans at most.
pub const MAX_ROOTS: usize = 40;
/// Working folders one listing takes at most.
pub const MAX_DIRS: usize = 200;
/// Skills one folder lists at most.
pub const MAX_PER_FOLDER: usize = 200;
/// Skill folders of one group (or of the canvas) at most.
pub const MAX_FOLDERS: usize = 20;
/// Skills one group plugin carries at most.
pub const MAX_GIVEN: usize = 100;
/// Bytes one group plugin copies at most (each skill also keeps the canvas
/// library's limits: 200 files, 8 MB, 4 levels).
pub const MAX_GIVEN_BYTES: u64 = 64 << 20;
/// Bytes of a SKILL.md read for its front matter.
pub const HEAD_BYTES: u64 = 8 << 10;
/// Folders between a working folder and its project root looked at, at most.
const MAX_LEVELS: usize = 32;

/// Where skills come from.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// The canvas plugin's skills (`<home>/plugin/skills`): every canvas session, all agents.
    Canvas,
    /// The canvas's skill library: canvas-wide agents get the skills assigned to them.
    Library,
    /// Skill folders of a group (`id`), or of every session (`id` empty):
    /// every session the canvas launches there, all agents.
    Group { id: String, name: String },
    /// A project (`root`: the folder holding `.git`, else the working folder):
    /// the folders its agents read between a session's folder and the root.
    Project { root: String },
    /// An agent's personal folders (and its CLI's own listing, merged in by the front end).
    Personal { agent: AgentKind },
}

/// A skill of a source.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct SourceSkill {
    /// Its front matter's `name`, else its folder's name.
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Its SKILL.md (empty: only its agent's CLI listed it).
    #[serde(default)]
    pub path: String,
    /// Agents that load it.
    #[serde(default)]
    pub agents: Vec<AgentKind>,
    /// The folder it was found in.
    #[serde(default)]
    pub folder: String,
    /// What is wrong with it (empty: nothing).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub problem: String,
    /// Managed by its agent's CLI (a personal skill of its listing): on or off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// The CLI listing's source (`personal`, `plugin`, `builtin`, …).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub managed: String,
}

/// One source and its skills.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SkillSource {
    pub kind: SourceKind,
    /// Its main folder (the plugin's skills, the project root, the first personal folder).
    pub path: String,
    /// Agents that load its skills.
    pub agents: Vec<AgentKind>,
    pub skills: Vec<SourceSkill>,
    /// The folders read, each with the number of skills found there.
    #[serde(default)]
    pub folders: Vec<(String, usize)>,
    /// What went wrong (a missing folder, …; empty: nothing).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub problem: String,
}

/// The skill folders of a group (`group`: its id) or of every session
/// (`group` empty), as `Request::SkillSources` sends them.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct FolderSet {
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub name: String,
    pub dirs: Vec<String>,
}

/// A two-letter tag for an agent (`CP`, `OC`, `CC`).
pub fn tag(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Copilot => "CP",
        AgentKind::OpenCode => "OC",
        AgentKind::Claude => "CC",
    }
}

/// Tags of agents, `all` for every agent.
pub fn tags(agents: &[AgentKind]) -> String {
    if ALL.iter().all(|k| agents.contains(k)) {
        return "all".into();
    }
    ALL.iter().filter(|k| agents.contains(k)).map(|k| tag(*k)).collect::<Vec<_>>().join(" ")
}

/// `base` joined with a relative path written with `/`.
pub fn rel(base: &Path, path: &str) -> PathBuf {
    path.split('/').filter(|p| !p.is_empty()).fold(base.to_path_buf(), |dir, part| dir.join(part))
}

/// Which of a working folder's levels a project folder is read at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// Every folder from the working folder up to the project root.
    Up,
    /// The working folder and the project root.
    Ends,
}

/// The project skill folders an agent reads by itself, as its docs say
/// (docs/AGENT-PROVIDERS.md, *Where agents find skills*).
pub fn project_folders(kind: AgentKind) -> &'static [(&'static str, Reach)] {
    match kind {
        // `.claude/skills` where it starts and in every parent up to the repository root.
        AgentKind::Claude => &[(".claude/skills", Reach::Up)],
        // Walks up from the working folder to the git worktree.
        AgentKind::OpenCode => &[(".opencode/skills", Reach::Up), (".opencode/skill", Reach::Up), (".claude/skills", Reach::Up), (".agents/skills", Reach::Up)],
        // The repository's three folders; `.github/skills` of parent folders too (monorepos).
        AgentKind::Copilot => &[(".github/skills", Reach::Up), (".agents/skills", Reach::Ends), (".claude/skills", Reach::Ends)],
    }
}

/// The folders personal skills live in: the user's profile, Claude Code's
/// folder (`CLAUDE_CONFIG_DIR`, else `~/.claude`) and Copilot's (`COPILOT_HOME`, else `~/.copilot`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Homes {
    pub user: PathBuf,
    pub claude: PathBuf,
    pub copilot: PathBuf,
}

impl Homes {
    /// The default folders under a profile folder.
    pub fn under(user: &Path) -> Self {
        Self { user: user.to_path_buf(), claude: user.join(".claude"), copilot: user.join(".copilot") }
    }
}

/// An agent's personal skill folders, as its docs say.
pub fn personal_folders(kind: AgentKind, homes: &Homes) -> Vec<PathBuf> {
    match kind {
        AgentKind::Claude => vec![homes.claude.join("skills")],
        AgentKind::OpenCode => vec![rel(&homes.user, ".config/opencode/skills"), rel(&homes.user, ".config/opencode/skill"), rel(&homes.user, ".claude/skills"), rel(&homes.user, ".agents/skills")],
        AgentKind::Copilot => vec![homes.copilot.join("skills"), rel(&homes.user, ".agents/skills")],
    }
}

/// The same path written another way (case, separators).
pub fn same_path(a: &Path, b: &Path) -> bool {
    crate::doc::same_dir(&a.to_string_lossy(), &b.to_string_lossy())
}

/// The project a folder belongs to: the nearest folder at or above it that
/// holds `.git` (a folder, or a worktree's file), else the folder itself.
pub fn project_root(dir: &Path) -> PathBuf {
    dir.ancestors().take(MAX_LEVELS).find(|d| d.join(".git").exists()).map(Path::to_path_buf).unwrap_or_else(|| dir.to_path_buf())
}

/// `dir` and its parents up to `root` (just `dir` when `root` is not above it).
fn levels(dir: &Path, root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for level in dir.ancestors().take(MAX_LEVELS) {
        out.push(level.to_path_buf());
        if same_path(level, root) {
            return out;
        }
    }
    vec![dir.to_path_buf()]
}

/// The project skill folders `kind` reads for a session in `dir` (some may not exist).
pub fn project_skill_dirs(kind: AgentKind, dir: &Path, root: &Path) -> Vec<PathBuf> {
    let levels = levels(dir, root);
    let mut out: Vec<PathBuf> = Vec::new();
    for (i, level) in levels.iter().enumerate() {
        let end = i == 0 || i + 1 == levels.len();
        for (path, reach) in project_folders(kind) {
            let folder = rel(level, path);
            if (*reach == Reach::Up || end) && !out.iter().any(|o| same_path(o, &folder)) {
                out.push(folder);
            }
        }
    }
    out
}

/// A skill found on disk.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub name: String,
    pub description: String,
    /// Its folder (the one holding SKILL.md).
    pub dir: PathBuf,
    pub problem: String,
}

impl Found {
    fn skill(&self, agents: &[AgentKind], folder: &Path) -> SourceSkill {
        SourceSkill {
            name: self.name.clone(),
            description: self.description.clone(),
            path: self.dir.join("SKILL.md").display().to_string(),
            agents: agents.to_vec(),
            folder: folder.display().to_string(),
            problem: self.problem.clone(),
            ..Default::default()
        }
    }
}

/// The start of a file (at most [`HEAD_BYTES`]), as text.
fn head(file: &Path) -> Option<String> {
    let mut bytes = Vec::new();
    std::fs::File::open(file).ok()?.take(HEAD_BYTES).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The skill in `dir` (a folder holding SKILL.md), named by its front matter.
fn found(dir: &Path) -> Option<Found> {
    let file = dir.join("SKILL.md");
    if !file.is_file() {
        return None;
    }
    let folder = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut skill = Found { name: folder, description: String::new(), dir: dir.to_path_buf(), problem: String::new() };
    match head(&file).as_deref().and_then(crate::canvas_wide::front_matter) {
        None => skill.problem = "SKILL.md has no front matter (name, description) at its start".into(),
        Some((name, description)) => {
            if !name.is_empty() {
                skill.name = name;
            }
            if description.is_empty() {
                skill.problem = "no description: agents decide from it when to use the skill".into();
            }
            skill.description = description;
        }
    }
    Some(skill)
}

/// The skills of a folder: the folders below it holding a SKILL.md (a few
/// levels deep, as `synced/<account>/<skill>` in Claude Code's; hidden
/// folders such as `.trash` skipped), by folder name, at most
/// [`MAX_PER_FOLDER`] (and [`MAX_VISITS`] folders looked at); with `single`,
/// a folder that holds a SKILL.md itself is that one skill.
pub fn scan_folder(dir: &Path, single: bool) -> Vec<Found> {
    scan(dir, single, true)
}

/// [`scan_folder`]; without `links`, linked folders below `dir` are not
/// looked into (a linked skill folder is still found, to be named).
fn scan(dir: &Path, single: bool, links: bool) -> Vec<Found> {
    if single && let Some(skill) = found(dir) {
        return vec![skill];
    }
    let mut out = Vec::new();
    walk(dir, SCAN_DEPTH, links, &mut 0, &mut out);
    out
}

/// Levels below a skills folder looked into.
const SCAN_DEPTH: usize = 3;
/// Folders one scan looks at, at most (a folder of many repositories stays quick).
pub const MAX_VISITS: usize = 2_000;

fn walk(dir: &Path, depth: usize, links: bool, visits: &mut usize, out: &mut Vec<Found>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut dirs: Vec<(PathBuf, bool)> = entries
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|e| e.file_type().ok().filter(|t| t.is_dir() || t.is_symlink()).map(|t| (e.path(), t.is_symlink())))
        .collect();
    dirs.sort_by_key(|(d, _)| d.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    for (d, link) in dirs {
        if out.len() >= MAX_PER_FOLDER || *visits >= MAX_VISITS {
            return;
        }
        *visits += 1;
        match found(&d) {
            Some(skill) => out.push(skill),
            None if depth > 1 && (links || !link) => walk(&d, depth - 1, links, visits, out),
            None => {}
        }
    }
}

/// Names (lowercase) of the skills `kind` loads by itself from the project
/// folders of a session started in `cwd` (none for a folder that is not a full path).
pub fn native_skill_names(kind: AgentKind, cwd: &Path) -> HashSet<String> {
    let mut names = HashSet::new();
    if !cwd.is_absolute() {
        return names;
    }
    let root = project_root(cwd);
    for folder in project_skill_dirs(kind, cwd, &root) {
        for skill in scan_folder(&folder, false) {
            names.insert(skill.name.to_lowercase());
            names.insert(skill.dir.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default());
        }
    }
    names
}

/// What the backend needs to list sources: the service's home, the personal
/// folders and the agents whose CLI is installed (all when empty).
#[derive(Clone, Debug, Default)]
pub struct Env {
    pub home: PathBuf,
    pub homes: Homes,
    pub installed: Vec<AgentKind>,
}

fn union(agents: &mut Vec<AgentKind>, more: &[AgentKind]) {
    for kind in more {
        if !agents.contains(kind) {
            agents.push(*kind);
        }
    }
    agents.sort();
}

/// A source made of folders whose skills every agent gets (the canvas plugin,
/// group folders): names that repeat are given once, the first folder's.
fn delivered(kind: SourceKind, dirs: &[PathBuf], single: bool) -> SkillSource {
    let mut source = SkillSource { kind, path: dirs.first().map(|d| d.display().to_string()).unwrap_or_default(), agents: ALL.to_vec(), skills: Vec::new(), folders: Vec::new(), problem: String::new() };
    let mut missing = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for dir in dirs {
        // A folder that is not a full path is not read (launches leave it out too).
        if !dir.is_absolute() || !dir.is_dir() {
            missing.push(dir.display().to_string());
            source.folders.push((dir.display().to_string(), 0));
            continue;
        }
        let found = scan_folder(dir, single);
        source.folders.push((dir.display().to_string(), found.len()));
        for skill in found {
            let mut entry = skill.skill(&ALL, dir);
            if !seen.insert(skill.name.to_lowercase()) {
                entry.problem = "an earlier folder has a skill of this name: not given".into();
            }
            source.skills.push(entry);
        }
    }
    if !missing.is_empty() {
        source.problem = format!("not found: {}", missing.join(", "));
    }
    source
}

/// Every source of a canvas's sessions: the canvas plugin, the skill folders
/// of every session and of each group (`folders`), the projects of the
/// working folders `dirs` (at most [`MAX_ROOTS`]), each installed agent's
/// personal folders and the canvas library (`canvas`: the canvas's id).
pub fn list(env: &Env, dirs: &[String], canvas: Option<&str>, folders: &[FolderSet]) -> Vec<SkillSource> {
    let mut out = vec![delivered(SourceKind::Canvas, &[env.home.join("plugin").join("skills")], false)];
    for set in folders.iter().filter(|s| !s.dirs.is_empty()).take(MAX_DIRS) {
        let dirs: Vec<PathBuf> = crate::doc::union_dirs(&set.dirs, &[]).into_iter().take(MAX_FOLDERS).map(PathBuf::from).collect();
        out.push(delivered(SourceKind::Group { id: set.group.clone(), name: set.name.clone() }, &dirs, true));
    }
    out.extend(projects(dirs));
    let installed = if env.installed.is_empty() { ALL.to_vec() } else { env.installed.clone() };
    out.extend(ALL.into_iter().filter(|k| installed.contains(k)).map(|kind| personal(kind, &env.homes)));
    if let Some(id) = canvas.filter(|c| crate::doc::valid_canvas_id(c)) {
        out.push(library(&crate::canvas_wide::canvas_dir(&env.home, id)));
    }
    out
}

/// The projects of working folders, in the order first seen.
fn projects(dirs: &[String]) -> Vec<SkillSource> {
    let mut roots: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
    for dir in crate::doc::union_dirs(dirs, &[]).into_iter().take(MAX_DIRS).map(PathBuf::from).filter(|d| d.is_absolute() && d.is_dir()) {
        let root = project_root(&dir);
        if let Some((_, working)) = roots.iter_mut().find(|(r, _)| same_path(r, &root)) {
            working.push(dir);
        } else if roots.len() < MAX_ROOTS {
            roots.push((root, vec![dir]));
        }
    }
    roots.iter().map(|(root, working)| project(root, working)).collect()
}

/// A project's skills: every folder an agent reads for a session in one of
/// the working folders, each skill with the agents that read its folder.
fn project(root: &Path, working: &[PathBuf]) -> SkillSource {
    let mut reads: Vec<(PathBuf, Vec<AgentKind>)> = Vec::new();
    for dir in working {
        for kind in ALL {
            for folder in project_skill_dirs(kind, dir, root) {
                match reads.iter_mut().find(|(f, _)| same_path(f, &folder)) {
                    Some((_, agents)) => union(agents, &[kind]),
                    None => reads.push((folder, vec![kind])),
                }
            }
        }
    }
    let path = root.display().to_string();
    let mut source = SkillSource { kind: SourceKind::Project { root: path.clone() }, path, agents: Vec::new(), skills: Vec::new(), folders: Vec::new(), problem: String::new() };
    for (folder, agents) in reads.iter().filter(|(f, _)| f.is_dir()) {
        let found = scan_folder(folder, false);
        source.folders.push((folder.display().to_string(), found.len()));
        for skill in found {
            union(&mut source.agents, agents);
            source.skills.push(skill.skill(agents, folder));
        }
    }
    source.skills.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.path.cmp(&b.path)));
    source
}

/// An agent's personal folders; each skill with every agent that reads its folder as a personal one.
fn personal(kind: AgentKind, homes: &Homes) -> SkillSource {
    let folders = personal_folders(kind, homes);
    let mut source = SkillSource {
        kind: SourceKind::Personal { agent: kind },
        path: folders.first().map(|f| f.display().to_string()).unwrap_or_default(),
        agents: vec![kind],
        skills: Vec::new(),
        folders: Vec::new(),
        problem: String::new(),
    };
    for folder in folders.iter().filter(|f| f.is_dir()) {
        let agents: Vec<AgentKind> = ALL.into_iter().filter(|k| personal_folders(*k, homes).iter().any(|f| same_path(f, folder))).collect();
        let found = scan_folder(folder, false);
        source.folders.push((folder.display().to_string(), found.len()));
        source.skills.extend(found.iter().map(|skill| skill.skill(&agents, folder)));
    }
    source
}

/// The canvas library (canvas-wide agents get the skills assigned to them).
fn library(canvas_dir: &Path) -> SkillSource {
    let dir = crate::canvas_wide::skills_dir(canvas_dir);
    let skills: Vec<SourceSkill> = crate::canvas_wide::list_skills(canvas_dir)
        .into_iter()
        .map(|s| SourceSkill { name: s.name, description: s.description, path: s.path, agents: ALL.to_vec(), folder: dir.display().to_string(), problem: s.problem, ..Default::default() })
        .collect();
    let folders = if dir.is_dir() { vec![(dir.display().to_string(), skills.len())] } else { Vec::new() };
    SkillSource { kind: SourceKind::Library, path: dir.display().to_string(), agents: ALL.to_vec(), skills, folders, problem: String::new() }
}

/// The name of the plugin that carries group skill folders to a launch.
pub const GROUP_PLUGIN: &str = "canvas-group-skills";

/// The folder of a service's group skill plugins (one per launch):
/// `<home>/group-skills/<instance>`, so services of other instances sharing
/// the home keep theirs.
pub fn group_plugins_dir(home: &Path, instance: &str) -> PathBuf {
    home.join("group-skills").join(folder_key(instance))
}

/// A folder name from an id: letters, digits, `-` and `_` kept, anything
/// else becomes `-` (at most 64 characters; empty: `none`).
pub fn folder_key(id: &str) -> String {
    let key: String = id.trim().chars().take(64).map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
    if key.trim_matches('-').is_empty() { "none".into() } else { key }
}

/// What [`prepare_group_plugin`] made.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GroupPlugin {
    /// The plugin folder (`None`: no skill to give).
    pub dir: Option<PathBuf>,
    /// The skills given, by name.
    pub given: Vec<String>,
    /// The skills left out, each with why.
    pub skipped: Vec<String>,
}

/// Builds the plugin that carries skill folders to one launch (`launch`, of
/// `group`): `<root>/<group>/<launch>/` (`root`: [`group_plugins_dir`]) with
/// `plugin.json` (Copilot CLI), `.claude-plugin/plugin.json` (Claude Code)
/// and a copy of each skill under `skills/` (OpenCode reads that folder),
/// built from scratch. Left out: folders that are not full paths and more
/// than twice [`MAX_FOLDERS`] (the canvas's and a group's), names the agent
/// already loads from the session's own project folders (`native`,
/// lowercase), names an earlier folder gave, links (the folders chosen
/// excepted: links below them are never followed), skills whose SKILL.md
/// does not fit the copy, and more than [`MAX_GIVEN`] skills or
/// [`MAX_GIVEN_BYTES`]; copies keep the canvas library's limits. With nothing
/// to give the folder is removed (and the group's, when empty).
pub fn prepare_group_plugin(root: &Path, group: &str, launch: &str, dirs: &[String], native: &HashSet<String>) -> std::io::Result<GroupPlugin> {
    let base = root.join(folder_key(group));
    let plugin = base.join(folder_key(launch));
    let mut result = GroupPlugin::default();
    let mut chosen: Vec<Found> = Vec::new();
    let mut names: HashSet<String> = HashSet::new();
    let dirs = crate::doc::union_dirs(&dirs[..dirs.len().min(4 * MAX_FOLDERS)], &[]);
    for (i, dir) in dirs.iter().enumerate() {
        let path = Path::new(dir);
        if i >= 2 * MAX_FOLDERS || !path.is_absolute() {
            let why = if i >= 2 * MAX_FOLDERS { "more folders than a launch takes" } else { "not a full path" };
            result.skipped.push(format!("folder {dir} ({why})"));
            continue;
        }
        for skill in scan(path, true, false) {
            let name = skill.name.to_lowercase();
            let folder = skill.dir.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
            let why = if folder.is_empty() {
                Some("its folder has no name")
            } else if native.contains(&name) || native.contains(&folder) {
                Some("the session's project already has it")
            } else if names.contains(&name) || names.contains(&folder) {
                Some("an earlier folder gave this name")
            } else if skill.dir != path && std::fs::symlink_metadata(&skill.dir).is_ok_and(|m| m.file_type().is_symlink()) {
                Some("a link: not copied")
            } else if chosen.len() >= MAX_GIVEN {
                Some("more than the plugin carries")
            } else {
                None
            };
            match why {
                Some(why) => result.skipped.push(format!("{} ({why})", skill.name)),
                None => {
                    names.insert(name);
                    names.insert(folder);
                    chosen.push(skill);
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&plugin);
    if chosen.is_empty() {
        let _ = std::fs::remove_dir(&base);
        return Ok(result);
    }
    let about = "Skill folders Copilot Canvas gives the sessions of a group.";
    let manifest = serde_json::json!({ "name": GROUP_PLUGIN, "description": about, "version": "1.0.0", "skills": "./skills/" });
    let claude = serde_json::json!({ "name": GROUP_PLUGIN, "description": about, "version": "1.0.0" });
    std::fs::create_dir_all(plugin.join(".claude-plugin"))?;
    std::fs::write(plugin.join("plugin.json"), serde_json::to_string_pretty(&manifest)?)?;
    std::fs::write(plugin.join(".claude-plugin").join("plugin.json"), serde_json::to_string_pretty(&claude)?)?;
    let out = plugin.join("skills");
    std::fs::create_dir_all(&out)?;
    let mut total = 0u64;
    for skill in chosen {
        if total >= MAX_GIVEN_BYTES {
            result.skipped.push(format!("{} (the plugin is full: {} MB)", skill.name, MAX_GIVEN_BYTES >> 20));
            continue;
        }
        let to = out.join(skill.dir.file_name().unwrap_or_default());
        let (mut files, mut bytes) = (0, 0);
        let copied = crate::canvas_wide::copy_tree(&skill.dir, &to, 0, &mut files, &mut bytes);
        // A SKILL.md that is a link, or past the limits, is not copied: no skill then.
        let copied = copied.and_then(|()| if to.join("SKILL.md").is_file() { Ok(()) } else { Err(std::io::Error::other("its SKILL.md was not copied (a link, or too large)")) });
        match copied {
            Ok(()) => {
                total += bytes;
                result.given.push(skill.name);
            }
            Err(error) => {
                let _ = std::fs::remove_dir_all(&to);
                result.skipped.push(format!("{} (cannot copy: {error})", skill.name));
            }
        }
    }
    if result.given.is_empty() {
        let _ = std::fs::remove_dir_all(&plugin);
        let _ = std::fs::remove_dir(&base);
        return Ok(result);
    }
    result.dir = Some(plugin);
    Ok(result)
}

/// Removes a launch's group skill plugin (made by [`prepare_group_plugin`]
/// under `root`) and its group's folder when that is left empty; a path not
/// inside `root` is left alone.
pub fn remove_group_plugin(root: &Path, plugin: &Path) {
    let inside = |p: &Path| p.starts_with(root) && p.components().count() > root.components().count();
    if !inside(plugin) || plugin.components().any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::CurDir)) {
        return;
    }
    let _ = std::fs::remove_dir_all(plugin);
    if let Some(group) = plugin.parent().filter(|g| inside(g)) {
        let _ = std::fs::remove_dir(group);
    }
}

/// A source's key, stable across listings: `canvas`, `library`,
/// `group:<id>`, `project:<root>`, `personal:<agent>`.
pub fn key(kind: &SourceKind) -> String {
    match kind {
        SourceKind::Canvas => "canvas".into(),
        SourceKind::Library => "library".into(),
        SourceKind::Group { id, .. } => format!("group:{id}"),
        SourceKind::Project { root } => format!("project:{}", root.to_lowercase()),
        SourceKind::Personal { agent } => format!("personal:{}", agent.id()),
    }
}

/// A source's name: the canvas, the group (`All sessions` for the canvas's
/// own folders), the project folder's name, the agent.
pub fn title(kind: &SourceKind) -> String {
    match kind {
        SourceKind::Canvas => "Canvas plugin".into(),
        SourceKind::Library => "Canvas library".into(),
        SourceKind::Group { id, .. } if id.is_empty() => "All sessions".into(),
        SourceKind::Group { name, .. } => name.clone(),
        SourceKind::Project { root } => root.trim_end_matches(['\\', '/']).rsplit(['\\', '/']).next().filter(|n| !n.is_empty()).unwrap_or(root).to_owned(),
        SourceKind::Personal { agent } => agent.label().into(),
    }
}

/// The same SKILL.md (or its folder) written another way.
fn same_skill_file(a: &str, b: &str) -> bool {
    let strip = |p: &str| {
        let p = p.trim().replace('/', "\\");
        let lower = p.to_lowercase();
        if lower.ends_with("\\skill.md") { p[..p.len() - 9].to_owned() } else { p }
    };
    !a.trim().is_empty() && !b.trim().is_empty() && crate::doc::same_dir(&strip(a), &strip(b))
}

/// Adds an agent's own listing (its CLI's skills with their state) to its
/// personal source (made when missing): skills of its folders gain their
/// state; skills only the CLI knows (plugins, built-in ones) are added,
/// loaded by that agent.
pub fn merge_listing(sources: &mut Vec<SkillSource>, agent: AgentKind, listing: &[crate::config::Skill]) {
    let personal = SourceKind::Personal { agent };
    let at = match sources.iter().position(|s| s.kind == personal) {
        Some(at) => at,
        None => {
            let at = sources.iter().rposition(|s| matches!(s.kind, SourceKind::Personal { .. })).map(|i| i + 1).or_else(|| sources.iter().position(|s| s.kind == SourceKind::Library)).unwrap_or(sources.len());
            sources.insert(at, SkillSource { kind: personal, path: String::new(), agents: vec![agent], skills: Vec::new(), folders: Vec::new(), problem: String::new() });
            at
        }
    };
    let source = &mut sources[at];
    for skill in listing {
        // Built-in skills have no file (OpenCode says `<builtin>`).
        let path = if skill.path.starts_with('<') { String::new() } else { skill.path.clone() };
        let managed = if skill.source.is_empty() { "listed".to_owned() } else { skill.source.clone() };
        let known = source.skills.iter().position(|s| s.enabled.is_none() && (same_skill_file(&s.path, &path) || (path.is_empty() && s.name.eq_ignore_ascii_case(&skill.name))));
        match known {
            Some(i) => {
                let entry = &mut source.skills[i];
                entry.enabled = Some(skill.enabled);
                entry.managed = managed;
            }
            None => source.skills.push(SourceSkill { name: skill.name.clone(), description: skill.description.clone(), path, agents: vec![agent], enabled: Some(skill.enabled), managed, ..Default::default() }),
        }
    }
}

/// The sources as one agent sees them (`None`: all): only the skills it
/// loads, and no other agent's personal source.
pub fn for_agent(sources: &[SkillSource], agent: Option<AgentKind>) -> Vec<SkillSource> {
    let Some(agent) = agent else { return sources.to_vec() };
    sources
        .iter()
        .filter(|s| !matches!(s.kind, SourceKind::Personal { agent: other } if other != agent))
        .map(|s| SkillSource { skills: s.skills.iter().filter(|k| k.agents.contains(&agent)).cloned().collect(), ..s.clone() })
        .collect()
}

/// Other skills of the same name that an agent loading `sources[source].skills[skill]`
/// also loads: (source, skill, the agents loading both). The canvas library
/// is left out (only canvas-wide agents get its skills).
pub fn same_name(sources: &[SkillSource], source: usize, skill: usize) -> Vec<(usize, usize, Vec<AgentKind>)> {
    let Some(this) = sources.get(source).and_then(|s| s.skills.get(skill)) else { return Vec::new() };
    if sources[source].kind == SourceKind::Library {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (i, other) in sources.iter().enumerate().filter(|(_, s)| s.kind != SourceKind::Library) {
        for (j, candidate) in other.skills.iter().enumerate() {
            if (i, j) == (source, skill) || !candidate.name.eq_ignore_ascii_case(&this.name) || (same_skill_file(&candidate.path, &this.path)) {
                continue;
            }
            let common: Vec<AgentKind> = this.agents.iter().filter(|k| candidate.agents.contains(k)).copied().collect();
            if !common.is_empty() {
                out.push((i, j, common));
            }
        }
    }
    out
}

/// Distinct agents, in [`ALL`] order.
pub fn agents_of(skills: &[SourceSkill]) -> Vec<AgentKind> {
    let set: BTreeSet<AgentKind> = skills.iter().flat_map(|s| s.agents.iter().copied()).collect();
    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cc-skills-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `dir/<name>/SKILL.md` with front matter.
    fn skill(dir: &Path, name: &str, description: &str) -> PathBuf {
        let folder = dir.join(name);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("SKILL.md"), format!("---\nname: {name}\ndescription: {description}\n---\n# {name}\n")).unwrap();
        folder
    }

    fn names(skills: &[SourceSkill]) -> Vec<&str> {
        skills.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn each_agent_reads_the_folders_its_docs_name() {
        let shown = |kind| project_folders(kind).iter().map(|(p, r)| format!("{p}{}", if *r == Reach::Ends { " (ends)" } else { "" })).collect::<Vec<_>>();
        assert_eq!(shown(AgentKind::Claude), [".claude/skills"]);
        assert_eq!(shown(AgentKind::OpenCode), [".opencode/skills", ".opencode/skill", ".claude/skills", ".agents/skills"]);
        assert_eq!(shown(AgentKind::Copilot), [".github/skills", ".agents/skills (ends)", ".claude/skills (ends)"]);
        let homes = Homes { user: PathBuf::from("/u"), claude: PathBuf::from("/c"), copilot: PathBuf::from("/p") };
        let personal = |kind| personal_folders(kind, &homes).iter().map(|p| p.to_string_lossy().replace('\\', "/")).collect::<Vec<_>>();
        assert_eq!(personal(AgentKind::Claude), ["/c/skills"], "CLAUDE_CONFIG_DIR, else ~/.claude");
        assert_eq!(personal(AgentKind::OpenCode), ["/u/.config/opencode/skills", "/u/.config/opencode/skill", "/u/.claude/skills", "/u/.agents/skills"]);
        assert_eq!(personal(AgentKind::Copilot), ["/p/skills", "/u/.agents/skills"], "COPILOT_HOME, else ~/.copilot; not ~/.claude");
        assert_eq!(Homes::under(Path::new("/u")).claude, Path::new("/u").join(".claude"));
        assert_eq!((tags(&ALL), tags(&[AgentKind::Claude, AgentKind::Copilot]), tags(&[])), ("all".into(), "CP CC".into(), String::new()));
    }

    #[test]
    fn project_folders_run_from_the_working_folder_to_the_repository_root() {
        let repo = temp("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let web = repo.join("packages").join("web");
        std::fs::create_dir_all(&web).unwrap();
        assert!(same_path(&project_root(&web), &repo), "the nearest folder holding .git");
        // A linked worktree has a .git file.
        let worktree = temp("worktree");
        std::fs::write(worktree.join(".git"), "gitdir: x").unwrap();
        assert!(same_path(&project_root(&worktree), &worktree));
        let plain = temp("plain");
        assert!(same_path(&project_root(&plain), &plain), "no repository: the folder itself");
        let shown = |kind| project_skill_dirs(kind, &web, &repo).iter().map(|p| p.strip_prefix(&repo).unwrap().to_string_lossy().replace('\\', "/")).collect::<Vec<_>>();
        assert_eq!(shown(AgentKind::Claude), ["packages/web/.claude/skills", "packages/.claude/skills", ".claude/skills"]);
        assert_eq!(
            shown(AgentKind::Copilot),
            ["packages/web/.github/skills", "packages/web/.agents/skills", "packages/web/.claude/skills", "packages/.github/skills", ".github/skills", ".agents/skills", ".claude/skills"],
            "the repository's three folders, parents' .github/skills"
        );
        assert_eq!(shown(AgentKind::OpenCode).len(), 12, "four folders at each of three levels");
        // A root that is not above the folder: only the folder itself.
        assert_eq!(project_skill_dirs(AgentKind::Claude, &plain, &repo), [plain.join(".claude").join("skills")]);
        for dir in [repo, worktree, plain] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn folders_list_skills_a_few_levels_deep_by_their_front_matter() {
        let dir = temp("scan");
        skill(&dir, "b-skill", "Second");
        skill(&dir, "a-skill", "First");
        skill(&dir.join("synced").join("account"), "deep", "Synced from an account");
        skill(&dir.join(".trash"), "gone", "Removed");
        std::fs::create_dir_all(dir.join("bare")).unwrap();
        std::fs::write(dir.join("bare").join("SKILL.md"), "# no front matter").unwrap();
        std::fs::create_dir_all(dir.join("renamed")).unwrap();
        std::fs::write(dir.join("renamed").join("SKILL.md"), "---\nname: other-name\n---\n").unwrap();
        let found = scan_folder(&dir, false);
        let summary: Vec<(&str, &str, bool)> = found.iter().map(|f| (f.name.as_str(), f.description.as_str(), f.problem.is_empty())).collect();
        assert_eq!(summary, [("a-skill", "First", true), ("b-skill", "Second", true), ("bare", "", false), ("other-name", "", false), ("deep", "Synced from an account", true)]);
        // A folder that is one skill; read only from the start of SKILL.md.
        let one = dir.join("a-skill");
        assert_eq!(scan_folder(&one, true).iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["a-skill"]);
        assert!(scan_folder(&one, false).is_empty(), "not as a folder of skills");
        std::fs::write(one.join("SKILL.md"), format!("---\nname: a-skill\ndescription: x\n{}---\n", "# padding\n".repeat(2_000))).unwrap();
        assert!(!scan_folder(&one, true)[0].problem.is_empty(), "front matter past the first 8 KB is not read");
        assert!(scan_folder(&dir.join("missing"), true).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A service home, a profile and a repository with a session folder in it.
    struct World {
        root: PathBuf,
        env: Env,
        repo: PathBuf,
        session: PathBuf,
        vault: PathBuf,
    }

    fn world() -> World {
        let root = temp("world");
        let home = root.join("home");
        skill(&home.join("plugin").join("skills"), "canvas-events", "Canvas events");
        let user = root.join("user");
        skill(&user.join(".claude").join("skills"), "mine", "Claude Code and OpenCode read it");
        skill(&user.join(".agents").join("skills"), "shared", "Copilot and OpenCode read it");
        skill(&rel(&user, ".config/opencode/skills"), "oc-only", "OpenCode only");
        skill(&user.join(".copilot").join("skills"), "cp-only", "Copilot only");
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        skill(&rel(&repo, ".claude/skills"), "review", "Project review");
        skill(&rel(&repo, ".agents/skills"), "release", "Project release");
        skill(&rel(&repo, ".github/skills"), "ci", "Project CI");
        let session = repo.join("app");
        skill(&rel(&session, ".claude/skills"), "app-only", "Only below app");
        let vault = root.join("vault");
        skill(&vault, "group-notes", "A group's skill");
        let env = Env { home, homes: Homes::under(&user), installed: vec![AgentKind::OpenCode, AgentKind::Claude] };
        World { root, env, repo, session, vault }
    }

    #[test]
    fn a_canvas_lists_every_source_with_the_agents_that_load_each_skill() {
        let w = world();
        let canvas = crate::canvas_wide::canvas_dir(&w.env.home, "c1");
        skill(&crate::canvas_wide::skills_dir(&canvas), "pr-etiquette", "PR feedback");
        let dirs = [w.session.display().to_string(), w.repo.display().to_string(), w.root.join("missing").display().to_string(), "relative".into()];
        let folders = [FolderSet { group: String::new(), name: String::new(), dirs: vec![w.vault.display().to_string()] }, FolderSet { group: "g1".into(), name: "Payments".into(), dirs: vec![] }];
        let sources = list(&w.env, &dirs, Some("c1"), &folders);
        let kinds: Vec<String> = sources.iter().map(|s| key(&s.kind)).collect();
        let project = format!("project:{}", w.repo.display().to_string().to_lowercase());
        assert_eq!(kinds, ["canvas", "group:", project.as_str(), "personal:opencode", "personal:claude", "library"], "groups without folders and uninstalled agents are left out");
        assert_eq!(names(&sources[0].skills), ["canvas-events"]);
        assert_eq!((sources[1].agents.clone(), names(&sources[1].skills)), (ALL.to_vec(), vec!["group-notes"]));
        // One project for both working folders, each skill with the agents that read its folder.
        let agents = |name: &str| sources[2].skills.iter().find(|s| s.name == name).map(|s| s.agents.clone()).unwrap_or_default();
        assert_eq!(agents("review"), [AgentKind::Copilot, AgentKind::OpenCode, AgentKind::Claude]);
        assert_eq!(agents("release"), [AgentKind::Copilot, AgentKind::OpenCode]);
        assert_eq!(agents("ci"), [AgentKind::Copilot]);
        assert_eq!(agents("app-only"), [AgentKind::Copilot, AgentKind::OpenCode, AgentKind::Claude], "sessions started in app read it");
        assert_eq!(title(&sources[2].kind), "repo");
        assert_eq!(sources[2].folders.len(), 4);
        // Personal folders: shared ones name every agent that reads them.
        let opencode = &sources[3];
        assert_eq!(names(&opencode.skills), ["oc-only", "mine", "shared"]);
        assert_eq!(opencode.skills[1].agents, [AgentKind::OpenCode, AgentKind::Claude]);
        assert_eq!(opencode.skills[2].agents, [AgentKind::Copilot, AgentKind::OpenCode]);
        assert_eq!(names(&sources[4].skills), ["mine"]);
        assert_eq!(names(&sources[5].skills), ["pr-etiquette"]);
        // No canvas id (or an invalid one): no library; every agent when none is known installed.
        let all = list(&Env { installed: Vec::new(), ..w.env.clone() }, &[], Some("../x"), &[]);
        assert_eq!(all.iter().map(|s| key(&s.kind)).collect::<Vec<_>>(), ["canvas", "personal:copilot", "personal:opencode", "personal:claude"]);
        assert_eq!(names(&all[1].skills), ["cp-only", "shared"]);
        let _ = std::fs::remove_dir_all(&w.root);
    }

    #[test]
    fn listings_scan_at_most_forty_projects_and_name_missing_group_folders() {
        let root = temp("many");
        let dirs: Vec<String> = (0..45).map(|i| { let d = root.join(format!("p{i}")); std::fs::create_dir_all(&d).unwrap(); d.display().to_string() }).collect();
        let sources = list(&Env { home: root.join("home"), ..Default::default() }, &dirs, None, &[]);
        assert_eq!(sources.iter().filter(|s| matches!(s.kind, SourceKind::Project { .. })).count(), MAX_ROOTS);
        let set = FolderSet { group: "g".into(), name: "G".into(), dirs: vec![root.join("nope").display().to_string(), root.join("p1").display().to_string(), "p2".into()] };
        let sources = list(&Env { home: root.join("home"), ..Default::default() }, &[], None, &[set]);
        let group = sources.iter().find(|s| key(&s.kind) == "group:g").unwrap();
        assert!(group.problem.contains("nope") && group.problem.contains("p2") && group.folders.len() == 3 && group.skills.is_empty(), "{group:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn group_plugins_copy_each_name_once_and_leave_out_what_the_project_has() {
        let w = world();
        let extra = w.root.join("extra");
        skill(&extra, "group-notes", "Same name as the vault's");
        let tools = skill(&extra, "tools", "Tools");
        std::fs::create_dir_all(tools.join("scripts")).unwrap();
        std::fs::write(tools.join("scripts").join("run.ps1"), "Write-Output ok").unwrap();
        skill(&extra, "review", "The project has one");
        let single = skill(&w.root, "one-skill", "A folder that is one skill");
        let dirs = [w.vault.display().to_string(), extra.display().to_string(), single.display().to_string(), w.vault.display().to_string()];
        // Claude Code in the repo already loads `review` from .claude/skills.
        let native = native_skill_names(AgentKind::Claude, &w.session);
        assert!(native.contains("review") && native.contains("app-only") && !native.contains("release"), "{native:?}");
        assert!(native_skill_names(AgentKind::Claude, Path::new("app")).is_empty() && native_skill_names(AgentKind::Claude, Path::new("")).is_empty(), "a relative folder reads nothing");
        let plugins = group_plugins_dir(&w.env.home, "default");
        assert_eq!(plugins, w.env.home.join("group-skills").join("default"), "one folder per service instance");
        let made = prepare_group_plugin(&plugins, "g-1", "box-1", &dirs, &native).unwrap();
        let plugin = made.dir.clone().expect("a plugin");
        assert_eq!(plugin, plugins.join("g-1").join("box-1"));
        assert_eq!(made.given, ["group-notes", "tools", "one-skill"]);
        assert_eq!(made.skipped, ["group-notes (an earlier folder gave this name)", "review (the session's project already has it)"]);
        let copilot: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(plugin.join("plugin.json")).unwrap()).unwrap();
        assert_eq!((copilot["name"].as_str(), copilot["skills"].as_str()), (Some(GROUP_PLUGIN), Some("./skills/")));
        let claude: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(plugin.join(".claude-plugin").join("plugin.json")).unwrap()).unwrap();
        assert_eq!(claude["name"], GROUP_PLUGIN);
        assert!(plugin.join("skills").join("tools").join("scripts").join("run.ps1").is_file() && plugin.join("skills").join("one-skill").join("SKILL.md").is_file());
        assert!(std::fs::read_to_string(plugin.join("skills").join("group-notes").join("SKILL.md")).unwrap().contains("A group's skill"), "the first folder's");
        // OpenCode in the repo loads release natively too; rebuilt, the plugin follows.
        let native = native_skill_names(AgentKind::OpenCode, &w.repo);
        let again = prepare_group_plugin(&plugins, "g-1", "box-1", &[extra.display().to_string()], &native).unwrap();
        assert_eq!(again.given, ["group-notes", "tools"]);
        assert!(!plugin.join("skills").join("one-skill").exists(), "rebuilt from scratch");
        // The source folders are only read.
        assert!(w.vault.join("group-notes").join("SKILL.md").is_file() && !w.vault.join("plugin.json").exists());
        // Nothing to give: no plugin, and no empty group folder.
        let none = prepare_group_plugin(&plugins, "g-1", "box-1", &[w.root.join("missing").display().to_string()], &HashSet::new()).unwrap();
        assert_eq!(none, GroupPlugin::default());
        assert!(!plugins.join("g-1").exists());
        // A launch's plugin goes when its terminal exits; nothing outside the root is ever removed.
        let made = prepare_group_plugin(&plugins, "", "box-2", &[w.vault.display().to_string()], &HashSet::new()).unwrap();
        assert_eq!(made.dir.as_deref(), Some(plugins.join("none").join("box-2").as_path()), "no group: the canvas's folders");
        for outside in [w.vault.clone(), plugins.clone(), plugins.join("none").join("..").join("..").join("..")] {
            remove_group_plugin(&plugins, &outside);
        }
        assert!(w.vault.join("group-notes").join("SKILL.md").is_file() && plugins.join("none").join("box-2").is_dir() && w.env.home.join("plugin").is_dir());
        remove_group_plugin(&plugins, &plugins.join("none").join("box-2"));
        assert!(!plugins.join("none").exists() && plugins.is_dir());
        assert_eq!((folder_key("g-1"), folder_key("../x y"), folder_key("")), ("g-1".into(), "---x-y".into(), "none".into()));
        let _ = std::fs::remove_dir_all(&w.root);
    }

    /// A link (a junction on Windows: no privilege needed) at `link` to the folder `target`.
    fn link_dir(link: &Path, target: &Path) -> bool {
        #[cfg(windows)]
        {
            std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link).arg(target).output().is_ok_and(|o| o.status.success())
        }
        #[cfg(not(windows))]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
    }

    #[test]
    fn group_plugins_never_follow_links_take_full_paths_only_and_need_their_skill_md() {
        let root = temp("links");
        let (home, outside, chosen) = (root.join("home"), root.join("outside"), root.join("chosen"));
        let plugins = group_plugins_dir(&home, "default");
        skill(&outside, "secret", "Outside the folder chosen");
        skill(&outside.join("deeper").join("more"), "far", "Two levels down, outside");
        skill(&chosen, "plain", "A skill of the folder");
        // A big subfolder sorted first never crowds out SKILL.md (files are copied first).
        let big = skill(&chosen, "big", "Many files");
        std::fs::create_dir_all(big.join("a-data")).unwrap();
        for i in 0..250 {
            std::fs::write(big.join("a-data").join(format!("{i}.txt")), "x").unwrap();
        }
        assert!(link_dir(&chosen.join("linked"), &outside.join("secret")) && link_dir(&chosen.join("via"), &outside.join("deeper")), "junctions need no privilege");
        let made = prepare_group_plugin(&plugins, "g", "t", &[chosen.display().to_string(), "relative\\skills".into()], &HashSet::new()).unwrap();
        let skills = made.dir.clone().expect("a plugin").join("skills");
        assert_eq!(made.given, ["big", "plain"]);
        assert!(skills.join("big").join("SKILL.md").is_file() && std::fs::read_dir(skills.join("big").join("a-data")).unwrap().count() < 250, "within the limits, SKILL.md first");
        assert!(made.skipped.contains(&r"folder relative\skills (not a full path)".to_owned()), "{:?}", made.skipped);
        // The listing follows links as the agents do; the copy never does.
        assert!(scan_folder(&chosen, true).iter().any(|s| s.name == "far"));
        assert!(made.skipped.contains(&"secret (a link: not copied)".to_owned()) && !made.skipped.iter().any(|s| s.starts_with("far")), "{:?}", made.skipped);
        assert!(!skills.join("linked").exists() && !skills.join("far").exists());
        // A folder chosen that is itself a link is read: the choice is yours.
        let made = prepare_group_plugin(&plugins, "g", "t2", &[chosen.join("linked").display().to_string()], &HashSet::new()).unwrap();
        assert_eq!(made.given, ["secret"]);
        // A folder without a name (a drive's root, `..`) gives no skill.
        let up = chosen.join("plain").join("..");
        std::fs::write(chosen.join("SKILL.md"), "---\nname: chosen\ndescription: d\n---\n").unwrap();
        let made = prepare_group_plugin(&plugins, "g", "t3", &[up.display().to_string()], &HashSet::new()).unwrap();
        assert_eq!((made.dir, made.skipped), (None, vec!["chosen (its folder has no name)".to_owned()]));
        // Scans are bounded however wide the folder is.
        let wide = root.join("wide");
        for i in 0..MAX_VISITS + 50 {
            std::fs::create_dir_all(wide.join(format!("d{i:05}"))).unwrap();
        }
        skill(&wide, "zz-last", "Past the budget");
        assert!(scan_folder(&wide, false).is_empty(), "at most {MAX_VISITS} folders looked at");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_group_plugin_carries_at_most_a_hundred_skills() {
        let root = temp("many-skills");
        let folder = root.join("skills");
        for i in 0..=MAX_GIVEN {
            skill(&folder, &format!("s{i:03}"), "One of many");
        }
        let made = prepare_group_plugin(&group_plugins_dir(&root.join("home"), "default"), "g", "t", &[folder.display().to_string()], &HashSet::new()).unwrap();
        assert_eq!((made.given.len(), made.skipped.clone()), (MAX_GIVEN, vec![format!("s{MAX_GIVEN:03} (more than the plugin carries)")]));
        // The canvas's and a group's folders: twice the folders of one list, the rest named.
        let many: Vec<String> = (0..=2 * MAX_FOLDERS).map(|i| root.join(format!("f{i}")).display().to_string()).collect();
        let made = prepare_group_plugin(&group_plugins_dir(&root.join("home"), "default"), "g", "t2", &many, &HashSet::new()).unwrap();
        assert_eq!(made.skipped, [format!("folder {} (more folders than a launch takes)", many[2 * MAX_FOLDERS])]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn listings_merge_into_personal_sources_and_filter_by_agent() {
        use crate::config::Skill;
        let file = |name: &str| format!(r"C:\u\.copilot\skills\{name}\SKILL.md");
        let mut sources = vec![
            SkillSource { kind: SourceKind::Canvas, path: String::new(), agents: ALL.to_vec(), skills: vec![SourceSkill { name: "review".into(), agents: ALL.to_vec(), ..Default::default() }], folders: Vec::new(), problem: String::new() },
            SkillSource {
                kind: SourceKind::Project { root: r"C:\r".into() },
                path: r"C:\r".into(),
                agents: Vec::new(),
                skills: vec![SourceSkill { name: "Review".into(), path: r"C:\r\.claude\skills\review\SKILL.md".into(), agents: vec![AgentKind::Claude, AgentKind::OpenCode], ..Default::default() }],
                folders: Vec::new(),
                problem: String::new(),
            },
            SkillSource { kind: SourceKind::Personal { agent: AgentKind::Copilot }, path: String::new(), agents: vec![AgentKind::Copilot], skills: vec![SourceSkill { name: "mine".into(), path: file("mine"), agents: vec![AgentKind::Copilot], ..Default::default() }], folders: Vec::new(), problem: String::new() },
            SkillSource { kind: SourceKind::Library, path: String::new(), agents: ALL.to_vec(), skills: vec![SourceSkill { name: "review".into(), agents: ALL.to_vec(), ..Default::default() }], folders: Vec::new(), problem: String::new() },
        ];
        let listing = [
            Skill { name: "mine".into(), source: "personal".into(), path: r"c:/U/.copilot/skills/mine".into(), enabled: false, ..Default::default() },
            Skill { name: "from-plugin".into(), source: "plugin".into(), path: String::new(), enabled: true, ..Default::default() },
        ];
        merge_listing(&mut sources, AgentKind::Copilot, &listing);
        let personal = &sources[2].skills;
        assert_eq!((personal[0].enabled, personal[0].managed.as_str()), (Some(false), "personal"), "its folder's skill gains its state");
        assert_eq!((personal[1].name.as_str(), personal[1].enabled, personal[1].agents.clone()), ("from-plugin", Some(true), vec![AgentKind::Copilot]));
        // An agent with no personal source gets one, before the library.
        merge_listing(&mut sources, AgentKind::Claude, &[Skill { name: "c".into(), path: "<builtin>".into(), enabled: true, ..Default::default() }]);
        assert_eq!(key(&sources[3].kind), "personal:claude");
        assert!(sources[3].skills[0].path.is_empty(), "built-in: no file");
        // One agent's view: what it loads, without other agents' personal sources.
        let claude = for_agent(&sources, Some(AgentKind::Claude));
        assert_eq!(claude.iter().map(|s| (key(&s.kind), s.skills.len())).collect::<Vec<_>>(), [("canvas".into(), 1), (r"project:c:\r".into(), 1), ("personal:claude".into(), 1), ("library".into(), 1)]);
        assert_eq!(for_agent(&sources, Some(AgentKind::Copilot))[1].skills.len(), 0, "Copilot does not read that folder here");
        assert_eq!(for_agent(&sources, None), sources);
        // The same name for an agent: the canvas plugin's and the project's (the library is apart).
        assert_eq!(same_name(&sources, 0, 0), [(1, 0, vec![AgentKind::OpenCode, AgentKind::Claude])]);
        assert!(same_name(&sources, 4, 0).is_empty() && same_name(&sources, 9, 0).is_empty());
        assert_eq!((title(&sources[1].kind), title(&SourceKind::Group { id: String::new(), name: String::new() })), ("r".into(), "All sessions".into()));
        assert_eq!(agents_of(&sources[1].skills), [AgentKind::OpenCode, AgentKind::Claude]);
    }

    #[test]
    fn sources_travel_as_json() {
        let source = SkillSource {
            kind: SourceKind::Group { id: "g1".into(), name: "Payments".into() },
            path: r"C:\s".into(),
            agents: ALL.to_vec(),
            skills: vec![SourceSkill { name: "x".into(), agents: vec![AgentKind::Claude], enabled: Some(true), ..Default::default() }],
            folders: vec![(r"C:\s".into(), 1)],
            problem: String::new(),
        };
        let json = serde_json::to_string(&source).unwrap();
        assert!(json.contains(r#""kind":{"Group":{"id":"g1","name":"Payments"}}"#) && json.contains(r#""agents":["claude"]"#), "{json}");
        assert_eq!(serde_json::from_str::<SkillSource>(&json).unwrap(), source);
        let lean: SkillSource = serde_json::from_str(r#"{"kind":"Canvas","path":"p","agents":[],"skills":[{"name":"y"}]}"#).unwrap();
        assert_eq!((lean.skills[0].enabled, lean.folders.len()), (None, 0), "optional fields may be left out");
    }
}
