//! Agent CLIs the canvas hosts: GitHub Copilot CLI, OpenCode and Claude Code.
//!
//! A [`Provider`] knows how to start, resume, fork and run one agent CLI, where
//! its session history lives and how to read status from it, and how to list
//! and change its configuration. Everything agent-specific goes through here.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use canvas_common::protocol::{AgentLaunch, SessionSummary};

pub mod claude;
pub mod claude_history;
pub mod config;
pub mod copilot;
pub mod opencode;
pub mod opencode_history;
pub mod opencode_server;
#[cfg(test)]
mod probes;
pub mod transcript;

pub use canvas_model::agent::{ALL, AgentKind, Caps, agent_of, agent_of_summary, native_id};

/// The provider of an agent kind (`kind.provider()`).
pub trait Provide {
    fn provider(self) -> &'static dyn Provider;
}

impl Provide for AgentKind {
    fn provider(self) -> &'static dyn Provider {
        match self {
            AgentKind::Copilot => &copilot::Copilot,
            AgentKind::OpenCode => &opencode::OpenCode,
            AgentKind::Claude => &claude::Claude,
        }
    }
}

/// The agent new sessions use when nothing else says: `COPILOT_CANVAS_AGENT`,
/// else the first installed of Copilot, OpenCode, Claude Code (Copilot when
/// none is).
pub fn default_agent() -> AgentKind {
    if let Some(kind) = agent_from_env() {
        return kind;
    }
    ALL.into_iter().find(|kind| kind.provider().program().is_ok()).unwrap_or(AgentKind::Copilot)
}

/// The machine's agent for new sessions: `COPILOT_CANVAS_AGENT`, else
/// settings.json `agent` in `home`, else the first installed agent.
pub fn machine_default(home: &Path) -> AgentKind {
    if let Some(kind) = agent_from_env() {
        return kind;
    }
    canvas_common::settings::Settings::load(home).agent.as_deref().and_then(AgentKind::parse).unwrap_or_else(default_agent)
}

/// Marks `COPILOT_CANVAS_AGENT` as a canvas session's own agent (same value).
const SESSION_AGENT: &str = "COPILOT_CANVAS_SESSION_AGENT";

/// The environment naming a canvas session's agent (sessions and passive
/// runs; `copilot-canvas note` records it as a note's `agent`):
/// `COPILOT_CANVAS_AGENT`, marked as the session's own so that a canvas
/// started inside the session does not take it as a choice for new sessions.
pub fn session_agent_env(kind: AgentKind) -> [(String, String); 2] {
    [("COPILOT_CANVAS_AGENT".to_owned(), kind.id().to_owned()), (SESSION_AGENT.to_owned(), kind.id().to_owned())]
}

/// `COPILOT_CANVAS_AGENT` as a choice for new sessions: not when it names
/// the agent its marker (`session`) says the canvas session runs (compared
/// as agents: `OpenCode` and `opencode` are one).
fn env_choice(value: Option<&str>, session: Option<&str>) -> Option<AgentKind> {
    let kind = AgentKind::parse(value?)?;
    (session.and_then(AgentKind::parse) != Some(kind)).then_some(kind)
}

/// `COPILOT_CANVAS_AGENT` when it chooses the agent for new sessions (not
/// when it only names the canvas session this process runs in).
pub fn agent_from_env() -> Option<AgentKind> {
    env_choice(std::env::var("COPILOT_CANVAS_AGENT").ok().as_deref(), std::env::var(SESSION_AGENT).ok().as_deref())
}

/// The agent of the canvas session this process runs in (its marker,
/// `COPILOT_CANVAS_SESSION_AGENT`; `None` outside one, or from a service
/// before it): never `COPILOT_CANVAS_AGENT` alone, which may be your choice.
pub fn session_agent() -> Option<AgentKind> {
    std::env::var(SESSION_AGENT).ok().as_deref().and_then(AgentKind::parse)
}

/// Agents whose CLI is installed.
pub fn installed() -> Vec<AgentKind> {
    ALL.into_iter().filter(|k| k.provider().program().is_ok()).collect()
}

/// The command line for a launch.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
    /// Environment; a key starting with '-' (empty value) removes that variable.
    pub env: Vec<(String, String)>,
}

/// Paths of the canvas integration the service provides to every session.
#[derive(Clone, Debug, Default)]
pub struct Integration {
    /// Canvas skills (`canvas-events`, `canvas-passive`).
    pub plugin_dir: Option<PathBuf>,
    /// Environment: `COPILOT_CANVAS_HOME`, `COPILOT_CANVAS_SESSION`, `PATH`
    /// (agent launches also `COPILOT_CANVAS_AGENT`: [`session_agent_env`]).
    pub env: Vec<(String, String)>,
    /// More plugins with skills (a canvas-wide agent's assigned canvas skills,
    /// a group's skill folders): another `--plugin-dir` for Copilot and Claude
    /// Code, their `skills` folders for OpenCode.
    pub skill_plugins: Vec<PathBuf>,
}

/// A group's folders, created on demand.
#[derive(Clone, Debug)]
pub struct GroupDirs {
    pub folder: PathBuf,
    pub ai_docs: PathBuf,
    pub outputs: PathBuf,
    /// Folder of `canvas-group.instructions.md`.
    pub instructions: PathBuf,
    pub instructions_file: PathBuf,
}

/// Creates the group's folders and its instructions file.
pub fn group_dirs(folder: &str, name: &str) -> Result<GroupDirs> {
    let group = canvas_model::doc::Group { name: name.to_owned(), folder: Some(folder.to_owned()), ..Default::default() };
    let instructions = canvas_model::doc::prepare_group_folders(&group)?.unwrap_or_else(|| PathBuf::from(folder).join(".canvas"));
    Ok(GroupDirs {
        folder: PathBuf::from(folder),
        ai_docs: group.ai_docs().unwrap_or_default(),
        outputs: group.outputs().unwrap_or_default(),
        instructions_file: instructions.join("canvas-group.instructions.md"),
        instructions,
    })
}

/// A source of session summaries (one per agent's history store).
pub trait History: Send {
    /// Refreshes metadata (everything every `full_every`, tracked sessions
    /// always) and live state of tracked sessions. Returns changed summaries
    /// and removed ids.
    fn poll(&mut self, tracked: &HashSet<String>, full_every: Duration) -> (Vec<SessionSummary>, Vec<String>);
    /// Pull-request and push activity found in tracked sessions:
    /// (session, event kind, data).
    fn take_detected(&mut self) -> Vec<(String, String, serde_json::Value)>;
    fn take_warnings(&mut self) -> Vec<String>;
}

/// One agent CLI.
pub trait Provider: Send + Sync {
    fn kind(&self) -> AgentKind;
    fn caps(&self) -> Caps;
    /// The executable to start (environment override, then PATH).
    fn program(&self) -> Result<String>;
    /// Whether `id` looks like one of this agent's session ids.
    fn is_session_id(&self, id: &str) -> bool;
    /// The command for a launch. `id` is the terminal id; the session is
    /// `launch.native_id` when set, else `id`.
    fn command(&self, id: &str, launch: &AgentLaunch, integration: &Integration) -> Result<Command>;
    /// Arguments a launch must never carry (they would select another session).
    fn reserved_args(&self) -> &'static [&'static str];
    /// The session's history store.
    fn history(&self) -> Box<dyn History>;
    /// Open in another process (never start a second copy).
    fn is_live(&self, native_id: &str) -> bool;
    /// The session's history exists (it can be resumed or forked).
    fn has_history(&self, native_id: &str) -> bool;
    /// Default root of the history store (shown by `doctor`).
    fn history_root(&self) -> PathBuf;
    /// A terminal that ran this agent exited: drop what its launch left
    /// behind. `home` is the canvas data folder.
    fn terminal_exited(&self, _id: &str, _home: &Path) {}
    /// A terminal that runs this agent is about to be asked to quit (it may
    /// block briefly, e.g. until nobody talks to the agent's own server).
    fn terminal_stopping(&self, _id: &str) {}
}

/// `program` from `env` or the first of `names` on PATH.
pub fn find_program(env: &str, names: &[&str]) -> Result<String> {
    if let Some(custom) = std::env::var_os(env) {
        let custom = custom.to_string_lossy().into_owned();
        ensure_not_batch(Path::new(&custom), env)?;
        return Ok(custom);
    }
    names
        .iter()
        .find_map(|name| crate::service::find_on_path(name))
        .or_else(|| names.iter().find_map(|name| find_installed(name)))
        .map(|p| p.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow::anyhow!("{} was not found on PATH (set {env} to override)", names[0]))
}

/// Where agent CLIs' own installers and the package managers put them on
/// macOS and Linux, searched after PATH: an app opened from Finder, the Dock
/// or a desktop launcher starts with a bare PATH, and some shells add these
/// folders only for interactive shells (MAC-LINUX-PLAN §5.2). None on Windows.
pub fn install_dirs(home: &Path) -> Vec<PathBuf> {
    if cfg!(windows) {
        return Vec::new();
    }
    let mut dirs: Vec<PathBuf> = [".local/bin", ".opencode/bin", ".bun/bin", ".npm-global/bin", ".volta/bin"].iter().map(|d| home.join(d)).collect();
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/home/linuxbrew/.linuxbrew/bin"].map(PathBuf::from));
    dirs
}

/// `name` in one of [`install_dirs`] of `home`.
pub fn find_installed_in(home: &Path, name: &str) -> Option<PathBuf> {
    install_dirs(home).into_iter().map(|dir| dir.join(name)).find(|path| path.is_file())
}

/// `name` in one of the user's [`install_dirs`].
pub fn find_installed(name: &str) -> Option<PathBuf> {
    find_installed_in(&home(), name)
}

/// True for `.cmd` / `.bat` files.
pub fn is_batch(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
}

/// Refuses batch scripts: CreateProcess runs them through cmd.exe, which
/// parses the command line differently from the quoting the PTY applies, so
/// text in an argument (a prompt) could run commands.
pub fn ensure_not_batch(program: &Path, env: &str) -> Result<()> {
    anyhow::ensure!(
        !is_batch(program),
        "{} is a batch script, which cannot be started safely; set {env} to the native executable",
        program.display()
    );
    Ok(())
}

/// Free text (prompt, title, name) as one argument that never parses as an
/// option: text starting with '-' gets a leading space.
pub fn positional(text: &str) -> String {
    if text.starts_with('-') { format!(" {text}") } else { text.to_owned() }
}

/// An option value such as a model or agent name: refused when it could parse
/// as an option or carries control characters.
pub fn option_value<'a>(option: &str, value: &'a str) -> Result<&'a str> {
    anyhow::ensure!(!value.starts_with('-'), "invalid {option} value {value:?}: must not start with '-'");
    anyhow::ensure!(!value.chars().any(char::is_control), "invalid {option} value: contains control characters");
    Ok(value)
}

/// The first 8 characters of an id (all of it when shorter), for messages.
pub fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

pub fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

pub fn exists(path: &Path) -> bool {
    path.exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_installed_outside_path_are_found_in_their_usual_folders() {
        let home = std::env::temp_dir().join(format!("cc-install-dirs-{}", uuid::Uuid::new_v4()));
        assert_eq!(find_installed_in(&home, "opencode"), None);
        if cfg!(windows) {
            assert!(install_dirs(&home).is_empty(), "Windows finds agents on PATH only");
            return;
        }
        // OpenCode's installer: ~/.opencode/bin, added to PATH by ~/.zshrc only.
        let bin = home.join(".opencode").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("opencode"), b"").unwrap();
        assert_eq!(find_installed_in(&home, "opencode"), Some(bin.join("opencode")));
        assert!(install_dirs(&home).contains(&PathBuf::from("/opt/homebrew/bin")), "Homebrew, which an app opened from the Dock does not have on PATH");
        assert_eq!(find_installed_in(&home, "a-directory-is-not-a-program"), None);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn free_text_and_option_values_never_become_options() {
        assert_eq!(positional("-v"), " -v");
        assert_eq!(positional("--help me"), " --help me");
        assert_eq!(positional("fix -v"), "fix -v");
        assert_eq!(option_value("--model", "opus").unwrap(), "opus");
        for bad in ["-x", "--evil", "a\nb", "a\u{1b}b"] {
            assert!(option_value("--model", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn batch_scripts_are_recognized_and_short_ids_never_panic() {
        for script in [r"C:\x\a.cmd", r"C:\x\a.BAT", "b.Cmd"] {
            assert!(is_batch(Path::new(script)) && ensure_not_batch(Path::new(script), "X").is_err(), "{script}");
        }
        assert!(!is_batch(Path::new(r"C:\x\a.exe")) && ensure_not_batch(Path::new("cmd"), "X").is_ok());
        assert_eq!((short_id("12345678-abcd"), short_id("abc"), short_id("")), ("12345678", "abc", ""));
        assert_eq!(short_id("\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}"), "\u{e9}\u{e9}\u{e9}\u{e9}", "never splits a character");
    }

    #[test]
    fn providers_report_the_model_caps() {
        for kind in ALL {
            assert_eq!(kind.provider().caps(), kind.caps(), "{kind:?}");
            assert_eq!(kind.provider().kind(), kind);
        }
    }

    #[test]
    fn a_sessions_own_agent_is_named_but_chooses_nothing() {
        let env = session_agent_env(AgentKind::OpenCode);
        assert_eq!(env[0], ("COPILOT_CANVAS_AGENT".to_owned(), "opencode".to_owned()));
        // Inside a canvas session the variable names that session's agent.
        assert_eq!(env_choice(Some(&env[0].1), Some(&env[1].1)), None);
        // Set by you (also inside a session, to another agent): a choice.
        assert_eq!(env_choice(Some("claude"), None), Some(AgentKind::Claude));
        assert_eq!(env_choice(Some("copilot"), Some("opencode")), Some(AgentKind::Copilot));
        assert_eq!((env_choice(None, Some("opencode")), env_choice(Some("nonsense"), None)), (None, None));
        // The same agent written another way is still the session's own, not a choice.
        assert_eq!(env_choice(Some(" OpenCode "), Some("opencode")), None);
        assert_eq!(env_choice(Some("claude-code"), Some("claude")), None);
        assert_eq!(env_choice(Some("claude"), Some("nonsense")), Some(AgentKind::Claude), "a marker naming no agent marks nothing");
    }
}
