//! VM sessions (docs/VM-SESSIONS.md): where a session runs, what clients
//! see of the VMs (`VmView`, in the bus snapshot), the operations windows
//! and the CLI ask for, and the operations agents in a guest relay to the
//! hub (`CliOp`). Pure and portable: the service (`canvas_agents::vm`)
//! does the work.
//!
//! VM sessions are optional; local stays the default. A box, a launch
//! default or a board runs on a VM only when it says so ([`Placement::Vm`]);
//! everything unset (old canvases included) is [`Placement::Local`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::agent::AgentKind;
use crate::path_style::PathStyle;

/// Where a session runs. Absent everywhere means [`Placement::Local`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(tag = "on", rename_all = "snake_case")]
pub enum Placement {
    /// This machine (the service's), as every session ran before VM sessions.
    #[default]
    Local,
    /// A VM of the configured pack.
    Vm(Box<VmPlacement>),
}

impl Placement {
    pub fn is_vm(&self) -> bool {
        matches!(self, Placement::Vm(_))
    }

    pub fn vm(&self) -> Option<&VmPlacement> {
        match self {
            Placement::Vm(vm) => Some(vm),
            Placement::Local => None,
        }
    }

    /// The placement of a box or a default as stored (`None`: local).
    pub fn of(stored: Option<&Placement>) -> Placement {
        stored.cloned().unwrap_or_default()
    }
}

/// A session on a VM: which repository (a key of the pack's repo catalog,
/// never a URL), which branch, which size and MCP preset, or the box whose
/// VM it shares (a subsession or a shell beside a VM session).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(default)]
pub struct VmPlacement {
    pub repo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp: Option<String>,
    /// The box whose VM this one runs in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub with: Option<String>,
}

/// The agent id a VM launch carries (`vm:copilot`): an older service, which
/// knows no VM sessions, refuses it as an unknown agent instead of starting
/// the agent on its own machine.
pub fn vm_agent(kind: AgentKind) -> String {
    format!("{VM_AGENT_PREFIX}{}", kind.id())
}

pub const VM_AGENT_PREFIX: &str = "vm:";

/// The agent of a `vm:<kind>` launch (`None`: not a VM launch).
pub fn parse_vm_agent(agent: &str) -> Option<AgentKind> {
    let rest = agent.trim();
    let rest = rest.get(..VM_AGENT_PREFIX.len()).filter(|p| p.eq_ignore_ascii_case(VM_AGENT_PREFIX)).map(|_| &rest[VM_AGENT_PREFIX.len()..])?;
    AgentKind::parse(rest)
}

/// What a VM session is doing, as the box and the ☁ VMs panel show it.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum VmState {
    /// Waiting to start (a budget or quota, with the reason in `VmInfo::note`).
    Queued,
    #[default]
    Claiming,
    Starting,
    Running,
    Suspending,
    Suspended,
    Resuming,
    /// The guest works but its link to this service is down (hub operations wait).
    IntegrationOffline,
    /// The VM is gone (work lives in pushed branches and the mirrored history).
    Lost,
    Deleting,
    Deleted,
    Failed,
}

impl VmState {
    pub fn word(self) -> &'static str {
        match self {
            VmState::Queued => "queued",
            VmState::Claiming => "claiming a VM",
            VmState::Starting => "starting",
            VmState::Running => "running",
            VmState::Suspending => "suspending",
            VmState::Suspended => "suspended",
            VmState::Resuming => "resuming",
            VmState::IntegrationOffline => "integration offline",
            VmState::Lost => "lost",
            VmState::Deleting => "deleting",
            VmState::Deleted => "deleted",
            VmState::Failed => "failed",
        }
    }
}

/// A box's VM.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct VmInfo {
    pub state: VmState,
    /// The sandbox (the provider's id), once claimed.
    pub sandbox: String,
    pub repo: String,
    pub branch: String,
    pub size: String,
    /// When its image was built (0: unknown).
    pub image_ms: u64,
    /// When it started running (0: not yet).
    pub started_ms: u64,
    /// Estimated cost so far, in cents of the pack's currency.
    pub cost_cents: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<GitState>,
    /// Since when hub operations wait (0: the link is up).
    pub offline_ms: u64,
    /// What it waits for or why it failed (one line).
    pub note: String,
    /// MCP and egress refusals so far.
    pub refusals: u32,
}

/// The guest's repository as the satellite last reported it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(default)]
pub struct GitState {
    pub branch: String,
    pub ahead: u32,
    pub behind: u32,
    pub dirty: u32,
    pub untracked: u32,
    pub stashes: u32,
    /// Local branches without an upstream, or ahead of it.
    pub unpushed: Vec<String>,
}

/// What deleting a VM would lose, from its repository's last report (the
/// quiescence barrier: a delete asks for these to be confirmed).
pub fn delete_findings(git: &GitState) -> Vec<String> {
    let mut out = Vec::new();
    let plural = |n: u32, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
    if !git.unpushed.is_empty() {
        out.push(format!("{} not pushed: {}", plural(git.unpushed.len() as u32, "branch", "branches"), git.unpushed.join(", ")));
    }
    if git.dirty > 0 {
        out.push(format!("{} with uncommitted changes", plural(git.dirty, "file", "files")));
    }
    if git.untracked > 0 {
        out.push(format!("{} not in git", plural(git.untracked, "new file", "new files")));
    }
    if git.stashes > 0 {
        out.push(plural(git.stashes, "stash", "stashes"));
    }
    out
}

/// A delete's confirmation that accepts losing whatever the barrier finds.
pub const DISCARD_ALL: &str = "*";

/// What clients see of VM sessions (`bus::Snapshot::vm`): absent without a
/// pack, so no VM UI shows.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct VmView {
    pub pack: PackSummary,
    /// Box (terminal) id → its VM.
    pub boxes: BTreeMap<String, VmInfo>,
    pub sandboxes: Vec<SandboxRow>,
    pub images: Vec<ImageRow>,
    pub budget: BudgetView,
    /// Starts waiting for a budget or quota.
    pub queued: Vec<QueuedStart>,
    /// The newest MCP and egress refusals.
    pub refusals: Vec<Refusal>,
}

/// The parts of the pack clients may know (names, never values).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct PackSummary {
    pub name: String,
    pub provider: String,
    pub repos: Vec<RepoSummary>,
    /// Size names, smallest first; the first is the default.
    pub sizes: Vec<String>,
    /// MCP presets guests may use.
    pub presets: Vec<String>,
    /// `pack check` found errors: VM starts are refused until they are fixed.
    pub errors: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct RepoSummary {
    pub key: String,
    pub default_branch: String,
    /// Globs of the service machine's folders that hold this repository.
    pub folders: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct SandboxRow {
    pub id: String,
    /// `session`, `pool`, `builder` or `orphan`.
    pub role: String,
    pub state: VmState,
    pub repo: String,
    pub size: String,
    /// The boxes it runs.
    pub boxes: Vec<String>,
    pub created_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct ImageRow {
    pub repo: String,
    pub id: String,
    pub built_ms: u64,
    /// `ready`, `building`, `failed`.
    pub state: String,
    pub note: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct BudgetView {
    pub running: u32,
    pub max_running: u32,
    pub cores: f32,
    pub max_cores: f32,
    pub today_cents: u64,
    pub daily_cents: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct QueuedStart {
    pub r#box: String,
    pub since_ms: u64,
    pub why: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Refusal {
    pub ms: u64,
    pub r#box: String,
    /// `mcp` or `egress`.
    pub kind: String,
    pub what: String,
    pub why: String,
}

/// What `atelier vm pack check` found (never a value of the pack).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct PackStatus {
    /// The pack's folder (`None`: no pack is configured).
    pub folder: Option<String>,
    pub findings: Vec<Finding>,
}

impl PackStatus {
    pub fn errors(&self) -> usize {
        self.findings.iter().filter(|f| f.level == Level::Error).count()
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    #[default]
    Info,
    Warning,
    Error,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Finding {
    pub level: Level,
    /// The pack's file, relative to its folder.
    pub file: String,
    /// 1-based; 0: the whole file.
    pub line: u32,
    pub text: String,
}

/// An operation on a box's VM (windows' bus actions and `Request::VmAction`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum VmOp {
    Suspend,
    Resume,
    /// A shell box in the same VM.
    Shell,
    /// A message to the agent: bring in the default branch.
    UpdateFromMain,
    /// Delete behind the quiescence barrier; `confirm` lists the findings
    /// the user accepted losing (empty: delete only when nothing is at risk).
    Delete {
        #[serde(default)]
        confirm: Vec<String>,
    },
    CancelStart,
    RetryStart,
}

/// A canvas command run in a guest, relayed to the hub and run there by the
/// same code as the box it ran in (docs/VM-SESSIONS-PLAN.md §4): the words,
/// the options and flags as the command line gave them, and the files the
/// command reads (`--body FILE`, `-` for standard input) inlined.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct CliOp {
    pub words: Vec<String>,
    pub options: Vec<(String, String)>,
    pub flags: Vec<String>,
    /// Files the command reads, by the name the command line used (`-`: standard input).
    pub inputs: Vec<Blob>,
}

/// The hub's answer to a [`CliOp`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct OpAnswer {
    pub ok: bool,
    /// `applied`, `queued` or `refused`.
    pub state: String,
    /// What the command prints (stdout when `ok`, the error otherwise).
    pub text: String,
    /// Files the command writes where it runs (`todo export --canvas FILE`).
    pub outputs: Vec<Blob>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Blob {
    pub name: String,
    #[serde(with = "crate::rpc::base64")]
    pub bytes: Vec<u8>,
}

/// Whether `rel` may name a file in a synced root (see `canvas_common::sat::name_ok`).
pub fn sync_name_ok(rel: &str) -> bool {
    canvas_common::sat::name_ok(rel)
}

/// The pack repository whose folder globs match `path` or one of its parent
/// folders (paths of the service's machine, in `style`).
pub fn repo_for_folder(repos: &[RepoSummary], path: &str, style: PathStyle) -> Option<String> {
    let normal = |p: &str| {
        let p = p.replace('\\', "/");
        let p = p.trim_end_matches('/').to_owned();
        if style == PathStyle::Windows { p.to_lowercase() } else { p }
    };
    let path = normal(path);
    if path.is_empty() {
        return None;
    }
    let mut candidates = vec![path.clone()];
    let mut current = path.as_str();
    while let Some((parent, _)) = current.rsplit_once('/') {
        if parent.is_empty() {
            break;
        }
        candidates.push(parent.to_owned());
        current = parent;
    }
    candidates.iter().find_map(|candidate| repos.iter().find(|r| r.folders.iter().any(|g| glob_match(&normal(g), candidate))).map(|r| r.key.clone()))
}

/// A glob over `/`-separated paths: `**` any number of components, `*` and
/// `?` within one component.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    fn parts(pattern: &[&str], path: &[&str]) -> bool {
        match pattern.split_first() {
            None => path.is_empty(),
            Some((&"**", rest)) => (0..=path.len()).any(|skip| parts(rest, &path[skip..])),
            Some((first, rest)) => path.split_first().is_some_and(|(head, tail)| component(first.as_bytes(), head.as_bytes()) && parts(rest, tail)),
        }
    }
    fn component(pattern: &[u8], text: &[u8]) -> bool {
        match pattern.split_first() {
            None => text.is_empty(),
            Some((b'*', rest)) => (0..=text.len()).any(|skip| component(rest, &text[skip..])),
            Some((b'?', rest)) => !text.is_empty() && component(rest, &text[1..]),
            Some((c, rest)) => text.first() == Some(c) && component(rest, &text[1..]),
        }
    }
    parts(&pattern, &path)
}

/// A git branch name for work on a todo: `atelier/DEC-031-short-title`.
pub fn branch_for_todo(id: &str, title: &str) -> String {
    let slug = slug(title, 40);
    let id = slug_keep_case(id);
    if slug.is_empty() { format!("atelier/{id}") } else { format!("atelier/{id}-{slug}") }
}

/// A git branch name for a session called `name`: `atelier/<name in lower case>`.
pub fn branch_for_session(name: &str) -> String {
    let slug = slug(name, 50);
    if slug.is_empty() { "atelier/session".to_owned() } else { format!("atelier/{slug}") }
}

/// Lower-case words of ASCII letters and digits joined by `-` (at most `max` bytes).
fn slug(text: &str, max: usize) -> String {
    let mut out = String::new();
    for word in text.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()) {
        let word = word.to_ascii_lowercase();
        if !out.is_empty() && out.len() + 1 + word.len() > max {
            break;
        }
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(&word[..word.len().min(max)]);
    }
    out
}

fn slug_keep_case(text: &str) -> String {
    text.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(20).collect()
}

/// Whether `name` is a branch name git accepts and that cannot read as an
/// option or a range (the rules of `git check-ref-format`, plus no leading `-`).
pub fn branch_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && !name.starts_with(['-', '/'])
        && !name.ends_with(['/', '.'])
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("@{")
        && !name.contains("//")
        && name != "@"
        && !name.split('/').any(|part| part.starts_with('.'))
        && !name.chars().any(|c| c.is_ascii_control() || c == ' ' || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\' | '\u{7f}'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deleting_names_what_would_be_lost() {
        assert!(delete_findings(&GitState { branch: "atelier/x".into(), ..Default::default() }).is_empty(), "all pushed and clean");
        let git = GitState { branch: "atelier/x".into(), ahead: 2, dirty: 3, untracked: 1, stashes: 2, unpushed: vec!["atelier/x".into()], ..Default::default() };
        assert_eq!(delete_findings(&git), ["1 branch not pushed: atelier/x", "3 files with uncommitted changes", "1 new file not in git", "2 stashes"]);
    }

    #[test]
    fn local_is_the_default_everywhere() {
        assert_eq!(Placement::default(), Placement::Local);
        assert_eq!(Placement::of(None), Placement::Local, "unset (old canvases) is local");
        assert!(!Placement::of(None).is_vm());
        let vm = Placement::Vm(Box::new(VmPlacement { repo: "contoso-app".into(), ..Default::default() }));
        assert_eq!(Placement::of(Some(&vm)), vm);
        // The JSON of both, and old shapes with missing parts.
        assert_eq!(serde_json::to_string(&Placement::Local).unwrap(), r#"{"on":"local"}"#);
        assert_eq!(serde_json::to_string(&vm).unwrap(), r#"{"on":"vm","repo":"contoso-app"}"#);
        assert_eq!(serde_json::from_str::<Placement>(r#"{"on":"vm"}"#).unwrap(), Placement::Vm(Box::default()));
        assert!(serde_json::from_str::<Placement>(r#"{"on":"mars"}"#).is_err());
    }

    #[test]
    fn vm_launches_name_an_agent_older_services_refuse() {
        assert_eq!(vm_agent(AgentKind::Copilot), "vm:copilot");
        assert_eq!(parse_vm_agent("vm:copilot"), Some(AgentKind::Copilot));
        assert_eq!(parse_vm_agent("VM:Claude"), Some(AgentKind::Claude));
        assert_eq!(parse_vm_agent("copilot"), None);
        assert_eq!(parse_vm_agent("vm:"), None);
        assert_eq!(parse_vm_agent("vm:mars"), None);
        assert_eq!(AgentKind::parse("vm:copilot"), None, "an older service does not know it");
    }

    #[test]
    fn folders_map_to_repositories_by_glob() {
        let repos = vec![
            RepoSummary { key: "app".into(), folders: vec!["**/contoso-app".into()], ..Default::default() },
            RepoSummary { key: "lib".into(), folders: vec![r"C:\src\lib-*".into(), "/home/*/src/lib".into()], ..Default::default() },
        ];
        assert_eq!(repo_for_folder(&repos, r"C:\Src\Contoso-App\sub\dir", PathStyle::Windows).as_deref(), Some("app"), "a folder inside it, any case on Windows");
        assert_eq!(repo_for_folder(&repos, "/Users/me/contoso-app", PathStyle::Posix).as_deref(), Some("app"));
        assert_eq!(repo_for_folder(&repos, "/Users/me/Contoso-App", PathStyle::Posix), None, "case counts on POSIX");
        assert_eq!(repo_for_folder(&repos, r"c:\src\lib-core", PathStyle::Windows).as_deref(), Some("lib"));
        assert_eq!(repo_for_folder(&repos, "/home/ana/src/lib/x", PathStyle::Posix).as_deref(), Some("lib"));
        assert_eq!(repo_for_folder(&repos, "/home/ana/src/other", PathStyle::Posix), None);
        assert_eq!(repo_for_folder(&repos, "", PathStyle::Posix), None);
        assert!(glob_match("a/**/b", "a/b") && glob_match("a/**/b", "a/x/y/b") && !glob_match("a/*/b", "a/x/y/b") && glob_match("a?c", "abc"));
    }

    #[test]
    fn branch_names_are_valid_git_refs() {
        assert_eq!(branch_for_todo("DEC-031", "Fix the parser: crash on `..` paths!"), "atelier/DEC-031-fix-the-parser-crash-on-paths");
        assert_eq!(branch_for_todo("WI-2", "  "), "atelier/WI-2");
        assert_eq!(branch_for_session("Review PR #42"), "atelier/review-pr-42");
        assert_eq!(branch_for_session("…"), "atelier/session");
        for name in [branch_for_todo("X-1", &"word ".repeat(30)), branch_for_session(&"ü".repeat(80)), branch_for_session("a.lock")] {
            assert!(branch_ok(&name), "{name}");
        }
        for bad in ["", "-x", "a..b", "a b", "a.lock", "a/", "x@{1}", "a:b", ".hidden", "a/.b", "a\\b", "a~1"] {
            assert!(!branch_ok(bad), "{bad:?}");
        }
    }

    #[test]
    fn relayed_operations_and_views_round_trip() {
        let op = CliOp { words: vec!["todo".into(), "submit".into(), "DEC-1".into()], options: vec![("summary".into(), "-".into())], flags: vec![], inputs: vec![Blob { name: "-".into(), bytes: vec![0, 255, 10] }] };
        let json = serde_json::to_string(&op).unwrap();
        assert!(json.contains(r#""bytes":"AP8K""#), "{json}");
        assert_eq!(serde_json::from_str::<CliOp>(&json).unwrap(), op);
        assert_eq!(serde_json::from_str::<CliOp>("{}").unwrap(), CliOp::default(), "every part optional");
        let mut view = VmView::default();
        view.boxes.insert("b".into(), VmInfo { state: VmState::Suspended, ..Default::default() });
        let json = serde_json::to_string(&view).unwrap();
        assert_eq!(serde_json::from_str::<VmView>(&json).unwrap(), view);
        assert_eq!(serde_json::to_string(&VmOp::Delete { confirm: vec![] }).unwrap(), r#"{"op":"delete","confirm":[]}"#);
        assert_eq!(serde_json::from_str::<VmOp>(r#"{"op":"delete"}"#).unwrap(), VmOp::Delete { confirm: vec![] });
        assert_eq!(VmState::IntegrationOffline.word(), "integration offline");
    }
}
