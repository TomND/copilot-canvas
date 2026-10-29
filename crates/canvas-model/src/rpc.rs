//! The backend API: every read and change a canvas front end needs from the
//! machine the agents run on (PLAN.md §6). The canvas document itself is not
//! here: it is owned by the service and synced with patches (see
//! [`crate::sync`]).
//!
//! One [`Request`] gets one [`Response`]. They travel as JSON
//! (`ClientMsg::Rpc` / `ServiceMsg::RpcReply`), so a request an older backend
//! does not know fails cleanly and new optional fields stay compatible.
//! `canvas_agents::backend` answers them in-process (local) and inside the
//! service (remote clients); both run the same handler.
//!
//! Calls block: front ends make slow ones on background threads, as they
//! always did, and quick ones (a folder listing) where they need the answer.

use std::time::Duration;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::agent::AgentKind;
use crate::automation::AutoLoad;
use crate::config::{Kind, Snapshot};
use crate::doc::{Doc, Group};
use crate::git::GitInfo;
use crate::migrate::Item;
use crate::vscode::{SyncCandidate, VsSession};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Request {
    /// Paths and agents of the backend's machine ([`BackendInfo`]).
    Info,

    /// An agent's configuration (always answered with `Config`; problems are
    /// listed in its `errors`).
    ConfigSnapshot { agent: AgentKind },
    /// Turns a plugin, skill or MCP server on or off.
    ConfigSetEnabled { agent: AgentKind, what: Kind, name: String, enabled: bool },
    /// Removes (uninstalls) a plugin, skill or MCP server.
    ConfigRemove { agent: AgentKind, what: Kind, name: String },
    /// `copilot plugin update NAME`.
    PluginUpdate { name: String },
    /// `copilot skill add SOURCE`.
    SkillAdd { source: String },
    /// `copilot mcp add …` from a `name -- command args` / `name URL` line.
    McpAdd { line: String },
    /// `copilot config KEY VALUE`.
    ConfigSet { key: String, value: String },
    /// `copilot config --rm KEY`.
    ConfigUnset { key: String },
    /// The command line that runs `copilot ARGS` in a canvas terminal (plugin
    /// installs ask questions, so they run in a normal host session).
    CliCommand { args: Vec<String> },
    /// Turns Copilot's mouse setting back on when an old service forced it
    /// off; answers whether it changed anything.
    RestoreMouse,

    /// Copilot's automations (`data.db`) plus, when `tasks`, the manifest
    /// folder (`dir`: the canvas setting) with Task Scheduler state.
    Automations { dir: Option<String>, tasks: bool },
    /// The manifest folder in use for a canvas setting.
    AutomationsDir { dir: Option<String> },
    /// Enables or disables a Copilot workflow in `data.db`.
    AutomationSetEnabled { key: String, enabled: bool },
    /// Enables or disables a manifest task: the manifest, then the task.
    TaskSetEnabled { name: String, manifest: String, enabled: bool },
    /// Runs a scheduled task now.
    TaskRun { name: String },
    /// The newest file matching each glob (patterns without a match are left out).
    NewestMatches { patterns: Vec<String> },
    /// The last bytes of files (log tails): (path, max bytes) each.
    Tails { files: Vec<(String, u64)> },

    /// VS Code agent sessions to sync; `created`: sessions the history says
    /// were made in VS Code; `since_ms`: Agents-window containers modified after.
    VsCodeScan { created: Vec<VsSession>, since_ms: u64 },
    /// What could move from VS Code to Copilot CLI (read-only).
    MigratePlan,
    /// Migrates the chosen new items (backs up Copilot's configuration first);
    /// answers the result lines.
    MigrateApply { items: Vec<Item> },

    /// Folder names in `dir` (".." first), or the drives when `None`.
    ListDir { dir: Option<String> },
    /// What a path is ([`PathInfo`]).
    PathInfo { path: String },
    /// Which of the paths are existing folders.
    DirsExist { paths: Vec<String> },
    /// A text file for the viewer, at most `max_bytes` from its start.
    ReadText { path: String, max_bytes: u64 },
    /// Shows a folder (or a file's folder) in File Explorer where the backend runs.
    OpenFolder { path: String },
    /// Opens an automation output, log or URL with its default program.
    OpenTarget { target: String },

    /// The Git repository a folder is in (`None`: not in one), with its branches.
    GitProbe { dir: String },
    /// `git worktree add`: `new_branch` from `base`, or `base` checked out.
    WorktreeAdd { root: String, path: String, new_branch: Option<String>, base: String },

    /// Whether an agent session has history (can be resumed or forked).
    HasHistory { agent: AgentKind, id: String },
    /// Copies a Copilot session's history to a new session id (fork).
    Fork { parent: String, child: String, name: String, cwd: Option<String> },
    /// A read-only transcript (`Response::Transcript`) of the agent session
    /// `session` (its own id). With `agent_key`: the subagent recorded under
    /// that key inside `session`'s history (Copilot's `agentId`, Claude
    /// Code's `Agent` tool call id). OpenCode's children are sessions of
    /// their own: their id with `parent` set. `cwd` helps find Claude Code
    /// transcripts; at most the newest `limit` entries. Nothing is started.
    Transcript {
        agent: AgentKind,
        session: String,
        #[serde(default)]
        parent: Option<String>,
        #[serde(default)]
        agent_key: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
        limit: u32,
    },

    /// Creates a group's `ai-docs`, `human-outputs` and instructions file.
    PrepareGroup { group: Group },
    /// The group's event catalog (created with the defaults when missing); answers its path.
    EnsureCatalog { folder: String },
    /// The automation doctor's default group folder (created); answers its path.
    DoctorFolder,
    /// Creates a passive session's files; answers the slug it got.
    CreatePassive { files: PassiveFiles },

    /// Writes a backup of a canvas next to its file; answers the backup's path.
    DocBackup { path: String, label: String, doc: Box<Doc> },
    /// Backups of a canvas, oldest first.
    DocBackups { path: String },

    /// Opens a one-time pairing code for a new remote client (`name`: what
    /// `remote clients` calls it); the service starts listening on its remote
    /// port if it was not. Only over the local pipe.
    PairCode { name: Option<String> },
    /// The paired remote clients. Only over the local pipe.
    RemoteClients,
    /// Revokes a paired client (id or name) and closes its connections. Only
    /// over the local pipe.
    RemoteRevoke { client: String },
    /// Puts an image (PNG, at most `clipimage::MAX_PNG`) on the clipboard of
    /// the service's machine (a remote image paste).
    SetClipboardImage {
        #[serde(with = "base64")]
        png: Vec<u8>,
    },
    /// One piece of a file dropped on a terminal in remote mode, written into
    /// `<home>/uploads/<date>/`: `data` (base64) goes at `offset` of upload
    /// `id`; the `last` piece answers the file's path (a free name from `name`).
    /// Files are at most [`MAX_UPLOAD`] bytes.
    Upload { name: String, id: String, offset: u64, data: String, last: bool },
    /// A folder's entries for the canvas's folder browser: `..`, then
    /// subfolders (ending in `\`), then files.
    ListEntries { dir: String },
    /// The reports inbox ([`crate::reports::Snapshot`]: newest first, with
    /// read state). Read only; the service pushes changes as `ServiceMsg::Reports`.
    Reports,
    /// One report's body ([`crate::reports::Content`]). Read only.
    ReportContent { id: String },
}

/// Largest file an upload writes.
pub const MAX_UPLOAD: u64 = 50 << 20;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Response {
    Done,
    /// The request failed; the text says why.
    Error(String),
    Text(String),
    Texts(Vec<String>),
    Flag(bool),
    Flags(Vec<bool>),
    Info(BackendInfo),
    Config(Box<Snapshot>),
    Automations(Box<AutoLoad>),
    VsSync { candidates: Vec<SyncCandidate>, warnings: Vec<String> },
    MigratePlan(Vec<Item>),
    Path(PathInfo),
    Git(Option<GitInfo>),
    Pairing(PairingCode),
    RemoteClients(Vec<RemoteClient>),
    Transcript(Box<crate::transcript::Transcript>),
    Reports(Box<crate::reports::Snapshot>),
    Report(Box<crate::reports::Content>),
}

/// An open pairing code (`Request::PairCode`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct PairingCode {
    /// `ABCD-EFGH-JKMN`.
    pub code: String,
    pub expires_ms: u64,
    /// Where the service listens for remote clients (`127.0.0.1:47600`).
    pub listen: String,
    /// The name the client gets (`None`: the client's own machine name).
    pub name: Option<String>,
}

/// A paired remote client (`Request::RemoteClients`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct RemoteClient {
    pub id: String,
    pub name: String,
    pub created_ms: u64,
    /// When it last authenticated (0: never).
    pub last_seen_ms: u64,
    /// When it was revoked (0: it is not).
    pub revoked_ms: u64,
    /// Connections it has open now.
    pub connections: u32,
    /// Who opened the code it paired with (a local process, or a command in a canvas terminal).
    #[serde(default)]
    pub paired_by: String,
    /// When that code was opened.
    #[serde(default)]
    pub code_opened_ms: u64,
}

/// `Vec<u8>` as a base64 string in JSON (images in requests).
mod base64 {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    pub fn decode(text: &str) -> Option<Vec<u8>> {
        let text = text.trim_end_matches('=');
        let mut out = Vec::with_capacity(text.len() * 3 / 4);
        let (mut bits, mut count) = (0u32, 0);
        for byte in text.bytes() {
            let value = ALPHABET.iter().position(|&a| a == byte)? as u32;
            bits = bits << 6 | value;
            count += 6;
            if count >= 8 {
                count -= 8;
                out.push((bits >> count) as u8);
                bits &= (1 << count) - 1;
            }
        }
        Some(out)
    }

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        decode(&text).ok_or_else(|| D::Error::custom("invalid base64"))
    }
}

/// The machine the backend runs on.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct BackendInfo {
    /// The canvas data folder (group folders, the doctor's folder, backups).
    pub home: String,
    /// The user's profile folder (where the folder browser starts).
    pub user_home: String,
    /// Copilot's session history.
    pub sessions_dir: String,
    /// Copilot's automation database.
    pub automation_db: String,
    /// The machine's agent for new sessions (environment, settings.json, else
    /// the first installed).
    pub default_agent: AgentKind,
    /// Agents whose CLI was found.
    pub installed: Vec<AgentKind>,
}

/// What a path is.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct PathInfo {
    pub exists: bool,
    pub dir: bool,
    pub file: bool,
    /// The absolute form of the path.
    pub absolute: String,
    /// It exists and is not an empty folder (an unreadable folder counts as occupied).
    pub occupied: bool,
}

/// A new passive session's files: `rules`, memory, run log and the group's
/// `GOALS.md` under `folder`, in the first free slug from `base_slug`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct PassiveFiles {
    pub folder: String,
    pub group_name: String,
    pub base_slug: String,
    /// Slugs already used by passive sessions of the group.
    pub taken: Vec<String>,
    pub name: String,
    pub rules: String,
}

impl Request {
    /// Short name for messages and logs.
    pub fn name(&self) -> &'static str {
        match self {
            Request::Info => "info",
            Request::ConfigSnapshot { .. } => "config snapshot",
            Request::ConfigSetEnabled { .. } => "config toggle",
            Request::ConfigRemove { .. } => "config remove",
            Request::PluginUpdate { .. } => "plugin update",
            Request::SkillAdd { .. } => "skill add",
            Request::McpAdd { .. } => "MCP add",
            Request::ConfigSet { .. } => "config set",
            Request::ConfigUnset { .. } => "config unset",
            Request::CliCommand { .. } => "CLI command",
            Request::RestoreMouse => "restore mouse",
            Request::Automations { .. } => "automations",
            Request::AutomationsDir { .. } => "automations folder",
            Request::AutomationSetEnabled { .. } => "automation toggle",
            Request::TaskSetEnabled { .. } => "task toggle",
            Request::TaskRun { .. } => "task run",
            Request::NewestMatches { .. } => "newest matches",
            Request::Tails { .. } => "tails",
            Request::VsCodeScan { .. } => "VS Code scan",
            Request::MigratePlan => "migration plan",
            Request::MigrateApply { .. } => "migration",
            Request::ListDir { .. } => "folder listing",
            Request::PathInfo { .. } => "path info",
            Request::DirsExist { .. } => "folder check",
            Request::ReadText { .. } => "read file",
            Request::OpenFolder { .. } => "open folder",
            Request::OpenTarget { .. } => "open",
            Request::GitProbe { .. } => "git probe",
            Request::WorktreeAdd { .. } => "worktree",
            Request::HasHistory { .. } => "history check",
            Request::Fork { .. } => "fork",
            Request::Transcript { .. } => "transcript",
            Request::PrepareGroup { .. } => "group folders",
            Request::EnsureCatalog { .. } => "event catalog",
            Request::DoctorFolder => "doctor folder",
            Request::CreatePassive { .. } => "passive session files",
            Request::DocBackup { .. } => "canvas backup",
            Request::DocBackups { .. } => "canvas backups",
            Request::PairCode { .. } => "pairing code",
            Request::RemoteClients => "remote clients",
            Request::RemoteRevoke { .. } => "revoke",
            Request::SetClipboardImage { .. } => "clipboard image",
            Request::Upload { .. } => "upload",
            Request::ListEntries { .. } => "folder entries",
            Request::Reports => "reports",
            Request::ReportContent { .. } => "report",
        }
    }

    /// Requests about the service's own remote access: answered by the
    /// service itself, only to clients on its local pipe.
    pub fn is_remote_admin(&self) -> bool {
        matches!(self, Request::PairCode { .. } | Request::RemoteClients | Request::RemoteRevoke { .. })
    }

    /// How long a remote caller waits for the answer (CLI runs can be slow).
    pub fn timeout(&self) -> Duration {
        let secs = match self {
            Request::PluginUpdate { .. } | Request::MigrateApply { .. } | Request::WorktreeAdd { .. } | Request::Fork { .. } => 900,
            Request::ConfigSnapshot { .. }
            | Request::ConfigSetEnabled { .. }
            | Request::ConfigRemove { .. }
            | Request::SkillAdd { .. }
            | Request::McpAdd { .. }
            | Request::ConfigSet { .. }
            | Request::ConfigUnset { .. }
            | Request::RestoreMouse
            | Request::MigratePlan => 300,
            Request::Automations { .. }
            | Request::VsCodeScan { .. }
            | Request::AutomationSetEnabled { .. }
            | Request::TaskSetEnabled { .. }
            | Request::TaskRun { .. }
            | Request::GitProbe { .. }
            | Request::SetClipboardImage { .. }
            | Request::Upload { .. }
            | Request::NewestMatches { .. } => 120,
            _ => 30,
        };
        Duration::from_secs(secs)
    }

    /// Answered quickly (file checks, listings): served apart from slow
    /// requests, so a front end waiting on one is never stuck behind CLI runs.
    pub fn is_quick(&self) -> bool {
        self.timeout() <= Duration::from_secs(30)
    }
}

/// The backend: in-process (`canvas_agents::backend::LocalBackend`) or the
/// service over the connection (`canvas_core::remote::RemoteBackend`).
pub trait Backend: Send + Sync {
    /// Runs one request. `Err`: the backend could not be asked (disconnected,
    /// timed out); a request that ran and failed answers `Response::Error`.
    fn call(&self, request: Request) -> Result<Response>;
}

/// A backend that is not there (tests, benchmarks): every call fails.
pub struct Offline;

impl Backend for Offline {
    fn call(&self, request: Request) -> Result<Response> {
        Err(anyhow!("no backend for {}", request.name()))
    }
}

/// Typed calls. `Response::Error` becomes `Err`.
impl dyn Backend + '_ {
    fn ask<T>(&self, request: Request, pick: impl FnOnce(Response) -> Option<T>) -> Result<T> {
        let name = request.name();
        match self.call(request)? {
            Response::Error(message) => Err(anyhow!(message)),
            other => pick(other).ok_or_else(|| anyhow!("unexpected answer to {name}")),
        }
    }

    fn done(&self, request: Request) -> Result<()> {
        self.ask(request, |r| matches!(r, Response::Done).then_some(()))
    }

    fn text(&self, request: Request) -> Result<String> {
        self.ask(request, |r| match r {
            Response::Text(text) => Some(text),
            _ => None,
        })
    }

    fn texts(&self, request: Request) -> Result<Vec<String>> {
        self.ask(request, |r| match r {
            Response::Texts(texts) => Some(texts),
            _ => None,
        })
    }

    fn flag(&self, request: Request) -> Result<bool> {
        self.ask(request, |r| match r {
            Response::Flag(flag) => Some(flag),
            _ => None,
        })
    }

    pub fn info(&self) -> Result<BackendInfo> {
        self.ask(Request::Info, |r| match r {
            Response::Info(info) => Some(info),
            _ => None,
        })
    }

    pub fn config(&self, agent: AgentKind) -> Result<Snapshot> {
        self.ask(Request::ConfigSnapshot { agent }, |r| match r {
            Response::Config(snapshot) => Some(*snapshot),
            _ => None,
        })
    }

    pub fn config_set_enabled(&self, agent: AgentKind, what: Kind, name: &str, enabled: bool) -> Result<String> {
        self.text(Request::ConfigSetEnabled { agent, what, name: name.to_owned(), enabled })
    }

    pub fn config_remove(&self, agent: AgentKind, what: Kind, name: &str) -> Result<String> {
        self.text(Request::ConfigRemove { agent, what, name: name.to_owned() })
    }

    pub fn plugin_update(&self, name: &str) -> Result<String> {
        self.text(Request::PluginUpdate { name: name.to_owned() })
    }

    pub fn skill_add(&self, source: &str) -> Result<String> {
        self.text(Request::SkillAdd { source: source.to_owned() })
    }

    pub fn mcp_add(&self, line: &str) -> Result<String> {
        self.text(Request::McpAdd { line: line.to_owned() })
    }

    pub fn config_set(&self, key: &str, value: &str) -> Result<String> {
        self.text(Request::ConfigSet { key: key.to_owned(), value: value.to_owned() })
    }

    pub fn config_unset(&self, key: &str) -> Result<String> {
        self.text(Request::ConfigUnset { key: key.to_owned() })
    }

    pub fn cli_command(&self, args: Vec<String>) -> Result<Vec<String>> {
        self.texts(Request::CliCommand { args })
    }

    pub fn restore_mouse(&self) -> Result<bool> {
        self.flag(Request::RestoreMouse)
    }

    pub fn automations(&self, dir: Option<String>, tasks: bool) -> Result<AutoLoad> {
        self.ask(Request::Automations { dir, tasks }, |r| match r {
            Response::Automations(load) => Some(*load),
            _ => None,
        })
    }

    pub fn automations_dir(&self, dir: Option<String>) -> Result<String> {
        self.text(Request::AutomationsDir { dir })
    }

    pub fn automation_set_enabled(&self, key: &str, enabled: bool) -> Result<()> {
        self.done(Request::AutomationSetEnabled { key: key.to_owned(), enabled })
    }

    pub fn task_set_enabled(&self, name: &str, manifest: &str, enabled: bool) -> Result<()> {
        self.done(Request::TaskSetEnabled { name: name.to_owned(), manifest: manifest.to_owned(), enabled })
    }

    pub fn task_run(&self, name: &str) -> Result<()> {
        self.done(Request::TaskRun { name: name.to_owned() })
    }

    pub fn newest_matches(&self, patterns: Vec<String>) -> Result<Vec<String>> {
        self.texts(Request::NewestMatches { patterns })
    }

    pub fn tails(&self, files: Vec<(String, u64)>) -> Result<Vec<String>> {
        self.texts(Request::Tails { files })
    }

    pub fn vscode_scan(&self, created: Vec<VsSession>, since_ms: u64) -> Result<(Vec<SyncCandidate>, Vec<String>)> {
        self.ask(Request::VsCodeScan { created, since_ms }, |r| match r {
            Response::VsSync { candidates, warnings } => Some((candidates, warnings)),
            _ => None,
        })
    }

    pub fn migrate_plan(&self) -> Result<Vec<Item>> {
        self.ask(Request::MigratePlan, |r| match r {
            Response::MigratePlan(items) => Some(items),
            _ => None,
        })
    }

    pub fn migrate_apply(&self, items: Vec<Item>) -> Result<Vec<String>> {
        self.texts(Request::MigrateApply { items })
    }

    pub fn list_dir(&self, dir: Option<String>) -> Result<Vec<String>> {
        self.texts(Request::ListDir { dir })
    }

    pub fn list_entries(&self, dir: &str) -> Result<Vec<String>> {
        self.texts(Request::ListEntries { dir: dir.to_owned() })
    }

    pub fn path_info(&self, path: &str) -> Result<PathInfo> {
        self.ask(Request::PathInfo { path: path.to_owned() }, |r| match r {
            Response::Path(info) => Some(info),
            _ => None,
        })
    }

    pub fn dirs_exist(&self, paths: Vec<String>) -> Result<Vec<bool>> {
        self.ask(Request::DirsExist { paths }, |r| match r {
            Response::Flags(flags) => Some(flags),
            _ => None,
        })
    }

    pub fn read_text(&self, path: &str, max_bytes: u64) -> Result<String> {
        self.text(Request::ReadText { path: path.to_owned(), max_bytes })
    }

    pub fn open_folder(&self, path: &str) -> Result<()> {
        self.done(Request::OpenFolder { path: path.to_owned() })
    }

    pub fn open_target(&self, target: &str) -> Result<()> {
        self.done(Request::OpenTarget { target: target.to_owned() })
    }

    pub fn git_probe(&self, dir: &str) -> Result<Option<GitInfo>> {
        self.ask(Request::GitProbe { dir: dir.to_owned() }, |r| match r {
            Response::Git(info) => Some(info),
            _ => None,
        })
    }

    pub fn worktree_add(&self, root: &str, path: &str, new_branch: Option<&str>, base: &str) -> Result<()> {
        self.done(Request::WorktreeAdd { root: root.to_owned(), path: path.to_owned(), new_branch: new_branch.map(str::to_owned), base: base.to_owned() })
    }

    pub fn has_history(&self, agent: AgentKind, id: &str) -> Result<bool> {
        self.flag(Request::HasHistory { agent, id: id.to_owned() })
    }

    pub fn fork(&self, parent: &str, child: &str, name: &str, cwd: Option<&str>) -> Result<()> {
        self.done(Request::Fork { parent: parent.to_owned(), child: child.to_owned(), name: name.to_owned(), cwd: cwd.map(str::to_owned) })
    }

    /// A read-only transcript (see [`Request::Transcript`]).
    pub fn transcript(&self, request: Request) -> Result<crate::transcript::Transcript> {
        anyhow::ensure!(matches!(request, Request::Transcript { .. }), "not a transcript request");
        self.ask(request, |r| match r {
            Response::Transcript(transcript) => Some(*transcript),
            _ => None,
        })
    }

    pub fn prepare_group(&self, group: &Group) -> Result<()> {
        self.done(Request::PrepareGroup { group: group.clone() })
    }

    pub fn ensure_catalog(&self, folder: &str) -> Result<String> {
        self.text(Request::EnsureCatalog { folder: folder.to_owned() })
    }

    pub fn doctor_folder(&self) -> Result<String> {
        self.text(Request::DoctorFolder)
    }

    pub fn create_passive(&self, files: PassiveFiles) -> Result<String> {
        self.text(Request::CreatePassive { files })
    }

    pub fn doc_backup(&self, path: &str, label: &str, doc: Doc) -> Result<String> {
        self.text(Request::DocBackup { path: path.to_owned(), label: label.to_owned(), doc: Box::new(doc) })
    }

    pub fn doc_backups(&self, path: &str) -> Result<Vec<String>> {
        self.texts(Request::DocBackups { path: path.to_owned() })
    }

    pub fn pair_code(&self, name: Option<String>) -> Result<PairingCode> {
        self.ask(Request::PairCode { name }, |r| match r {
            Response::Pairing(code) => Some(code),
            _ => None,
        })
    }

    pub fn remote_clients(&self) -> Result<Vec<RemoteClient>> {
        self.ask(Request::RemoteClients, |r| match r {
            Response::RemoteClients(list) => Some(list),
            _ => None,
        })
    }

    pub fn remote_revoke(&self, client: &str) -> Result<String> {
        self.text(Request::RemoteRevoke { client: client.to_owned() })
    }

    pub fn set_clipboard_image(&self, png: Vec<u8>) -> Result<()> {
        self.done(Request::SetClipboardImage { png })
    }

    /// Sends one upload piece (see [`Request::Upload`]); the last answers the path.
    pub fn upload_piece(&self, name: &str, id: &str, offset: u64, data: String, last: bool) -> Result<Option<String>> {
        self.ask(Request::Upload { name: name.to_owned(), id: id.to_owned(), offset, data, last }, |r| match r {
            Response::Done if !last => Some(None),
            Response::Text(path) if last => Some(Some(path)),
            _ => None,
        })
    }

    /// The reports inbox (see [`Request::Reports`]).
    pub fn reports(&self) -> Result<crate::reports::Snapshot> {
        self.ask(Request::Reports, |r| match r {
            Response::Reports(snapshot) => Some(*snapshot),
            _ => None,
        })
    }

    /// One report's body (see [`Request::ReportContent`]).
    pub fn report_content(&self, id: &str) -> Result<crate::reports::Content> {
        self.ask(Request::ReportContent { id: id.to_owned() }, |r| match r {
            Response::Report(content) => Some(*content),
            _ => None,
        })
    }
}

/// The answer to a request this build does not know (from a newer client).
pub fn unsupported(error: &str) -> Response {
    Response::Error(format!("the service does not support this request (update it): {error}"))
}

/// Answers a request given as JSON with JSON. A request this build does not
/// know is answered with `Response::Error`.
pub fn answer_json(json: &str, handle: impl FnOnce(Request) -> Response) -> String {
    let response = match serde_json::from_str::<Request>(json) {
        Ok(request) => handle(request),
        Err(error) => unsupported(&error.to_string()),
    };
    serde_json::to_string(&response).unwrap_or_else(|e| format!("{{\"Error\":\"cannot encode the answer: {e}\"}}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    impl Backend for Echo {
        fn call(&self, request: Request) -> Result<Response> {
            Ok(match request {
                Request::Info => Response::Info(BackendInfo { home: "h".into(), installed: vec![AgentKind::Claude], ..Default::default() }),
                Request::ListDir { .. } => Response::Error("denied".into()),
                Request::GitProbe { .. } => Response::Text("wrong".into()),
                _ => Response::Done,
            })
        }
    }

    #[test]
    fn typed_calls_map_answers_and_errors() {
        let backend: &dyn Backend = &Echo;
        assert_eq!(backend.info().unwrap().installed, vec![AgentKind::Claude]);
        assert_eq!(backend.list_dir(None).unwrap_err().to_string(), "denied");
        assert!(backend.git_probe("x").unwrap_err().to_string().contains("unexpected answer to git probe"));
        backend.task_run("t").unwrap();
        let offline: &dyn Backend = &Offline;
        assert!(offline.info().is_err());
    }

    #[test]
    fn requests_travel_as_json_and_unknown_ones_fail_cleanly() {
        let request = Request::WorktreeAdd { root: "r".into(), path: "p".into(), new_branch: None, base: "main".into() };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
        let reply = answer_json(&json, |r| if r == request { Response::Done } else { Response::Error("x".into()) });
        assert_eq!(serde_json::from_str::<Response>(&reply).unwrap(), Response::Done);
        let reply = answer_json(r#"{"FutureThing":{"x":1}}"#, |_| Response::Done);
        assert!(matches!(serde_json::from_str::<Response>(&reply).unwrap(), Response::Error(e) if e.contains("does not support")));
        assert!(Request::MigrateApply { items: vec![] }.timeout() > Request::Info.timeout());
        // Transcripts travel as JSON too; older clients may leave the optional fields out.
        let request = Request::Transcript { agent: AgentKind::OpenCode, session: "ses_c".into(), parent: Some("ses_p".into()), agent_key: None, cwd: None, limit: 50 };
        assert_eq!(serde_json::from_str::<Request>(&serde_json::to_string(&request).unwrap()).unwrap(), request);
        assert!(request.is_quick());
        let short = serde_json::from_str::<Request>(r#"{"Transcript":{"agent":"copilot","session":"x","limit":5}}"#).unwrap();
        assert!(matches!(short, Request::Transcript { parent: None, agent_key: None, cwd: None, limit: 5, .. }));
    }

    #[test]
    fn images_travel_as_base64_and_admin_requests_are_marked() {
        for len in 0..10 {
            let bytes: Vec<u8> = (0..len as u8).map(|b| b.wrapping_mul(97)).collect();
            assert_eq!(base64::decode(&base64::encode(&bytes)).unwrap(), bytes);
        }
        assert_eq!(base64::encode(b"Man"), "TWFu");
        assert_eq!(base64::encode(b"Ma"), "TWE=");
        assert_eq!(base64::decode("TWE="), Some(b"Ma".to_vec()));
        assert_eq!(base64::decode("T!=="), None);
        let request = Request::SetClipboardImage { png: vec![0x89, b'P', b'N', b'G', 0, 255] };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(json, r#"{"SetClipboardImage":{"png":"iVBORwD/"}}"#);
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
        assert!(!request.is_quick() && !request.is_remote_admin());
        assert!(Request::PairCode { name: None }.is_remote_admin() && Request::RemoteClients.is_remote_admin() && Request::RemoteRevoke { client: "x".into() }.is_remote_admin());
        let reply = Response::Pairing(PairingCode { code: "ABCD-EFGH-JKMN".into(), expires_ms: 1, listen: "127.0.0.1:47600".into(), name: None });
        assert_eq!(serde_json::from_str::<Response>(&serde_json::to_string(&reply).unwrap()).unwrap(), reply);
    }

    #[test]
    fn report_requests_and_answers_round_trip_and_old_services_say_so() {
        for request in [Request::Reports, Request::ReportContent { id: "0001700000000000-abcd1234".into() }] {
            let json = serde_json::to_string(&request).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
            assert!(request.is_quick() && !request.is_remote_admin());
            // A service from before reports answers that it does not support them.
            let reply = answer_json(&json.replace("Report", "FutureReport"), |_| Response::Done);
            assert!(matches!(serde_json::from_str::<Response>(&reply).unwrap(), Response::Error(e) if e.contains("does not support")));
        }
        let content = crate::reports::Content { id: "1-a".into(), format: crate::reports::Format::Markdown, body: "# Brief\n\"quoted\" ✓".into() };
        let reply = Response::Report(Box::new(content.clone()));
        assert_eq!(serde_json::from_str::<Response>(&serde_json::to_string(&reply).unwrap()).unwrap(), reply);
        struct Inbox;
        impl Backend for Inbox {
            fn call(&self, request: Request) -> Result<Response> {
                Ok(match request {
                    Request::Reports => Response::Reports(Box::new(crate::reports::Snapshot { version: 2, unread: 1, ..Default::default() })),
                    Request::ReportContent { id } if id == "1-a" => Response::Report(Box::new(crate::reports::Content { id, ..Default::default() })),
                    _ => Response::Error("no such report".into()),
                })
            }
        }
        let backend: &dyn Backend = &Inbox;
        assert_eq!(backend.reports().unwrap().unread, 1);
        assert_eq!(backend.report_content("1-a").unwrap().id, "1-a");
        assert_eq!(backend.report_content("2-b").unwrap_err().to_string(), "no such report");
    }
}
