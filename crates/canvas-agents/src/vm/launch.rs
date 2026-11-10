//! VM launches: the durable intent a VM box's launch leaves under
//! `<home>/vm/launches/<box>.json`, the agent's command line in the guest,
//! and `atelier vm attach` (the thin lane's terminal program: it claims a VM
//! through the pack's hooks, then runs the hooks' shell into the guest,
//! where the agent runs).
//!
//! The service never provisions on a client's thread: a VM launch records
//! its intent and starts `atelier vm attach` in the box's terminal at once;
//! the terminal shows each step.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use canvas_common::protocol::AgentLaunch;
use canvas_model::agent::AgentKind;
use canvas_model::vm::{VmPlacement, VmState};
use serde::{Deserialize, Serialize};

use crate::paths::now_ms;

pub const INTENT_VERSION: u32 = 1;

/// A VM box's launch, kept across service restarts.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Intent {
    pub version: u32,
    pub r#box: String,
    pub agent: String,
    pub placement: VmPlacement,
    /// The agent's command line in the guest (the guest's own `copilot`).
    pub argv: Vec<String>,
    /// Its environment in the guest.
    pub env: Vec<(String, String)>,
    pub state: VmState,
    pub sandbox: String,
    /// The repository's folder in the guest, once claimed.
    pub cwd: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub error: String,
    /// Bumped by a cancel or retry: a step started before it never writes its result.
    pub fence: u64,
    /// Failed steps in a row (three fail the launch).
    pub attempts: u32,
    /// The satellite credential minted for its sandbox (`sat-…`).
    pub satellite: String,
    /// An operation asked for and not done yet: `suspend`, `resume` or `delete`.
    pub pending: String,
    /// Since when it waits (queued) or why (`note`).
    pub queued_ms: u64,
    pub note: String,
    /// The generation of the box's terminal that runs in the guest behind
    /// the satellite (0: none); a restarted service adopts it again.
    #[serde(skip_serializing_if = "is_zero")]
    pub terminal: u64,
    /// The box's group (its name and folder on the hub): its notes and
    /// instructions are synced into the guest.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub group_name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub group_folder: String,
    /// The plugin with the group's skill folders, made on the hub for this box.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub group_plugin: String,
    /// Resume on a new VM: the sandbox whose mirrored history holds the
    /// conversation (`resume_session`), put into the new guest at its start.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub resume_from: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub resume_session: String,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

static INTENTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Changes box `id`'s intent under the intents' lock (read, change, write):
/// the service's threads never write over each other's changes.
pub fn update<T>(home: &Path, id: &str, change: impl FnOnce(&mut Intent) -> T) -> Result<Option<T>> {
    let _turn = INTENTS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut intent) = load(home, id)? else { return Ok(None) };
    let out = change(&mut intent);
    save(home, &mut intent)?;
    Ok(Some(out))
}

/// Writes `intent` under the intents' lock.
pub fn store(home: &Path, intent: &mut Intent) -> Result<()> {
    let _turn = INTENTS.lock().unwrap_or_else(|e| e.into_inner());
    save(home, intent)
}

/// Whether `id` may name a box's files (a terminal id: letters, digits, `-`, `_`).
pub fn valid_box(id: &str) -> bool {
    !id.is_empty() && id.len() <= 100 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn launches_dir(home: &Path) -> PathBuf {
    home.join("vm").join("launches")
}

pub fn intent_path(home: &Path, id: &str) -> Result<PathBuf> {
    anyhow::ensure!(valid_box(id), "not a box id: {id:?}");
    Ok(launches_dir(home).join(format!("{id}.json")))
}

pub fn load(home: &Path, id: &str) -> Result<Option<Intent>> {
    match std::fs::read(intent_path(home, id)?) {
        Ok(bytes) => {
            let intent: Intent = serde_json::from_slice(&bytes).context("unreadable VM launch")?;
            anyhow::ensure!(intent.version <= INTENT_VERSION, "this VM launch was written by a newer Atelier");
            Ok(Some(intent))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn save(home: &Path, intent: &mut Intent) -> Result<()> {
    intent.version = INTENT_VERSION;
    intent.updated_ms = now_ms();
    crate::paths::write_atomic(&intent_path(home, &intent.r#box)?, &serde_json::to_vec_pretty(intent)?)?;
    Ok(())
}

/// Every recorded VM launch.
pub fn all(home: &Path) -> Vec<Intent> {
    let Ok(entries) = std::fs::read_dir(launches_dir(home)) else { return Vec::new() };
    entries.flatten().filter_map(|e| std::fs::read(e.path()).ok()).filter_map(|b| serde_json::from_slice::<Intent>(&b).ok()).filter(|i| valid_box(&i.r#box)).collect()
}

/// The agent's command line and environment in the guest. The canvas's
/// folders of this machine (group folders, extra folders, plugins) do not
/// exist there, so the launch goes without them; the canvas integration in
/// the guest comes from the satellite (later phases).
pub fn guest_command(kind: AgentKind, id: &str, launch: &AgentLaunch, target: &super::target::ExecTarget) -> Result<crate::agents::ArgsEnv> {
    let mut launch = launch.clone();
    launch.group_folder = None;
    launch.group_name = None;
    launch.add_dirs.clear();
    launch.headless_prompt = None;
    anyhow::ensure!(launch.fork_from.is_none(), "forking a session that runs on a VM is not available");
    let roots = match target {
        super::target::ExecTarget::Vm(roots) => roots.clone(),
        super::target::ExecTarget::Local => super::target::GuestRoots::default(),
    };
    // The synced roots in this machine's places: plugins, the group's notes and instructions.
    let plugins = roots.plugins(id);
    let integration = crate::agents::Integration { plugin_dir: None, env: Vec::new(), skill_plugins: plugins.iter().map(std::path::PathBuf::from).collect() };
    let (args, mut env) = match kind {
        AgentKind::Copilot => {
            let (mut args, mut env) = crate::agents::copilot::args(id, &launch, &integration)?;
            if !launch.resume {
                for dir in roots.add_dirs() {
                    args.extend(["--add-dir".to_owned(), dir]);
                }
            }
            let instructions = roots.instructions();
            if !instructions.is_empty() {
                env.push(("COPILOT_CUSTOM_INSTRUCTIONS_DIRS".to_owned(), instructions.join(",")));
            }
            (args, env)
        }
        AgentKind::Claude => {
            // This machine's folders, its MCP file and its group folder are not the guest's: the synced roots stand in.
            let guest_launch = AgentLaunch { group_folder: None, group_name: None, mcp_off: Vec::new(), add_dirs: Vec::new(), ..launch.clone() };
            let home = crate::agents::claude::ClaudeHome { dir: PathBuf::new(), user_config: PathBuf::new() };
            let (mut args, env) = crate::agents::claude::Claude.args_env(&home, id, &guest_launch, &integration)?;
            // Nothing below is saved with a Claude session: every run gets it.
            for dir in roots.add_dirs() {
                args.extend(["--add-dir".to_owned(), dir]);
            }
            if let Some(file) = roots.instructions_file() {
                args.extend(["--append-system-prompt-file".to_owned(), file]);
            }
            if roots.mcp {
                args.extend(["--strict-mcp-config".to_owned(), "--mcp-config".to_owned(), super::target::guest(super::sync::CLAUDE_MCP)]);
            }
            // Windows Terminal's variables are this machine's matter.
            (args, env.into_iter().filter(|(k, _)| !k.starts_with('-')).collect())
        }
        other => bail!("{} sessions cannot run on a VM in this version: its history is a database and its live state a server on the machine it runs on, which the canvas cannot follow from a VM yet (Copilot CLI and Claude Code can)", other.label()),
    };
    // The canvas's own variables are set by the guest's side.
    env.retain(|(k, _)| !canvas_common::env::is_ours(k));
    // `atelier note path` in the guest: the group's notes as synced there.
    if let Some(group) = &roots.group {
        env.push(("ATELIER_NOTES".to_owned(), super::target::guest(&format!("notes/{group}"))));
    }
    let mut argv = vec![kind.id().to_owned()];
    argv.extend(args);
    Ok((argv, env))
}

/// The environment a VM session's agent gets in the guest from the canvas.
pub fn guest_env(id: &str, kind: AgentKind) -> Vec<(String, String)> {
    let mut env = vec![("ATELIER_SESSION".to_owned(), id.to_owned()), ("ATELIER_REMOTE".to_owned(), "satellite-lite".to_owned())];
    env.extend(crate::agents::session_agent_env(kind));
    env
}

/// `atelier vm attach --box ID`: the terminal of a VM box. The service's
/// launcher claims and prepares its VM (this shows each step); then the
/// pack's `shell-command` hook's command runs here: the agent in the guest.
/// With `provider: local` (tests and CI) the agent runs in the sandbox
/// directly, as one of its processes, until the satellite link (Phase 2)
/// carries every provider's terminals.
pub fn attach(home: &Path, id: &str) -> Result<()> {
    use std::io::Write;
    let folder = super::pack::configured(home).context("no VM pack is configured: VM sessions are off")?;
    let (pack, status) = super::pack::load(&folder);
    anyhow::ensure!(status.errors() == 0, "the VM pack has {} error(s): see `atelier vm pack check`", status.errors());
    let hooks = super::hooks::Hooks::of(&pack);
    let say = |text: &str| {
        let mut out = std::io::stdout();
        let _ = write!(out, "\x1b[2m☁ {text}\x1b[0m\r\n");
        let _ = out.flush();
    };
    let mut last = String::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30 * 60);
    let intent = loop {
        let intent = load(home, id)?.with_context(|| format!("no VM launch for box {id}"))?;
        let line = match intent.state {
            VmState::Queued => format!("Waiting to start: {}", intent.note),
            VmState::Claiming if intent.sandbox.is_empty() => format!("Claiming a VM for {} on {}…", intent.placement.repo, intent.placement.branch.as_deref().unwrap_or("its branch")),
            VmState::Claiming | VmState::Starting => format!("Preparing the VM ({})…", intent.sandbox),
            VmState::Resuming | VmState::Suspended | VmState::Suspending => "Resuming the VM…".to_owned(),
            VmState::Running if !intent.sandbox.is_empty() => break intent,
            VmState::Failed => bail!("the VM could not be started: {}", intent.error),
            VmState::Deleted | VmState::Deleting => bail!("this session's VM was deleted"),
            VmState::Lost => bail!("this session's VM is gone (its work is in its pushed branch)"),
            other => format!("{}…", other.word()),
        };
        if line != last {
            say(&line);
            last = line;
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "the VM did not start within 30 minutes");
        std::thread::sleep(std::time::Duration::from_millis(250));
    };
    let mut cmd = if pack.provider == super::pack::ProviderKind::Local {
        say(&format!("Joining the VM ({})…", intent.sandbox));
        let local = super::local::Local::new(super::local_root(home, &pack));
        let cmd = local.command(&intent.sandbox, &intent.argv, Some(intent.cwd.as_str()).filter(|c| !c.is_empty()), &intent.env)?;
        // This process (and so the agent it becomes or starts) is the sandbox's: suspend and delete reach it.
        local.track(&intent.sandbox, std::process::id())?;
        cmd
    } else {
        shell_command(&hooks, &intent, &say)?
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = cmd.exec();
        bail!("cannot start the shell into the VM: {error}");
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status().context("cannot start the shell into the VM")?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// The pack's `shell-command` hook's command into the VM of `intent`.
fn shell_command(hooks: &super::hooks::Hooks, intent: &Intent, say: &dyn Fn(&str)) -> Result<std::process::Command> {
    anyhow::ensure!(hooks.has("shell-command"), "the VM pack has no shell-command hook: this VM's terminal comes with the satellite link");
    say(&format!("Joining the VM ({})…", intent.sandbox));
    let answer = hooks.call("shell-command", &serde_json::json!({ "sandbox": intent.sandbox, "box": intent.r#box, "argv": intent.argv, "env": intent.env.iter().cloned().collect::<std::collections::BTreeMap<_, _>>(), "cwd": intent.cwd }))?;
    let argv: Vec<String> = answer["argv"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
    anyhow::ensure!(!argv.is_empty(), "the shell-command hook gave no command");
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intents_are_kept_by_box_and_never_outside_their_folder() {
        let home = std::env::temp_dir().join(format!("atelier-intents-{}", uuid::Uuid::new_v4()));
        let mut intent = Intent { r#box: "12345678-1234-1234-1234-123456789abc".into(), agent: "copilot".into(), ..Default::default() };
        save(&home, &mut intent).unwrap();
        assert_eq!(load(&home, &intent.r#box).unwrap(), Some(intent.clone()));
        assert_eq!(all(&home).len(), 1);
        assert!(intent_path(&home, "../x").is_err() && intent_path(&home, "a/b").is_err() && intent_path(&home, "").is_err());
        assert_eq!(load(&home, "missing").unwrap(), None);
        std::fs::write(intent_path(&home, "newer").unwrap(), r#"{"version": 99}"#).unwrap();
        assert!(load(&home, "newer").is_err(), "a newer intent is never read as ours");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn the_guest_command_carries_nothing_of_this_machine() {
        let id = "12345678-1234-1234-1234-123456789abc";
        let launch = AgentLaunch {
            agent: "copilot".into(),
            name: Some("Fix it".into()),
            prompt: Some("go".into()),
            allow_all: true,
            add_dirs: vec![r"C:\shared".into()],
            group_folder: Some(r"C:\groups\g".into()),
            group_name: Some("G".into()),
            env: vec![("ATELIER_HOME".into(), r"C:\h".into()), ("KEEP".into(), "1".into())],
            ..Default::default()
        };
        let (argv, env) = guest_command(AgentKind::Copilot, id, &launch, &crate::vm::target::ExecTarget::Local).unwrap();
        assert_eq!(argv[..3], ["copilot", "--session-id", id]);
        assert!(argv.windows(2).any(|w| w == ["-i", "go"]) && argv.contains(&"--allow-all".to_owned()));
        let all = format!("{argv:?}{env:?}");
        assert!(!all.contains(r"C:\") && !all.contains("--plugin-dir") && !all.contains("--add-dir") && !all.contains("ATELIER_HOME"), "{all}");
        assert!(env.contains(&("KEEP".into(), "1".into())));
        let refused = guest_command(AgentKind::OpenCode, id, &launch, &crate::vm::target::ExecTarget::Local).unwrap_err().to_string();
        assert!(refused.contains("cannot run on a VM"), "{refused}");
        assert!(guest_command(AgentKind::Copilot, id, &AgentLaunch { fork_from: Some("p".into()), ..launch.clone() }, &crate::vm::target::ExecTarget::Local).is_err(), "no forks of VM sessions");
        let env = guest_env(id, AgentKind::Copilot);
        assert!(env.contains(&("ATELIER_SESSION".into(), id.into())) && env.contains(&("ATELIER_REMOTE".into(), "satellite-lite".into())));
        // With a satellite: the synced roots in this machine's places, guest paths only.
        let roots = crate::vm::target::GuestRoots { plugin: true, profile: true, profile_instructions: false, group: Some("g".into()), group_skills: true, mcp: false };
        let (argv, env) = guest_command(AgentKind::Copilot, id, &launch, &crate::vm::target::ExecTarget::Vm(roots)).unwrap();
        let plugins: Vec<&str> = argv.windows(2).filter(|w| w[0] == "--plugin-dir").map(|w| w[1].as_str()).collect();
        assert_eq!(plugins, ["%ROOTS%/plugin", "%ROOTS%/profile", format!("%ROOTS%/group-skills/{id}").as_str()]);
        let dirs: Vec<&str> = argv.windows(2).filter(|w| w[0] == "--add-dir").map(|w| w[1].as_str()).collect();
        assert_eq!(dirs, ["%ROOTS%/notes/g/ai-docs", "%ROOTS%/notes/g/human-outputs"]);
        assert!(env.contains(&("COPILOT_CUSTOM_INSTRUCTIONS_DIRS".into(), "%ROOTS%/instructions/g".into())));
        assert!(env.contains(&("ATELIER_NOTES".into(), "%ROOTS%/notes/g".into())));
        assert!(!format!("{argv:?}{env:?}").contains(r"C:\"), "nothing of this machine");
        // Claude Code: its own flags for the same roots, the box's MCP preset as a file of the roots.
        let roots = crate::vm::target::GuestRoots { plugin: true, profile: false, profile_instructions: false, group: Some("g".into()), group_skills: false, mcp: true };
        let claude = AgentLaunch { agent: "claude".into(), group_folder: Some(r"C:\work\g".into()), group_name: Some("g".into()), mcp_off: vec!["gh".into()], add_dirs: vec![r"C:\x".into()], ..launch.clone() };
        let (argv, env) = guest_command(AgentKind::Claude, id, &claude, &crate::vm::target::ExecTarget::Vm(roots)).unwrap();
        assert_eq!(argv[..3], ["claude", "--session-id", id]);
        let pairs = |flag: &str| argv.windows(2).filter(|w| w[0] == flag).map(|w| w[1].clone()).collect::<Vec<_>>();
        assert_eq!(pairs("--plugin-dir"), ["%ROOTS%/plugin"]);
        assert_eq!(pairs("--add-dir"), ["%ROOTS%/notes/g/ai-docs", "%ROOTS%/notes/g/human-outputs"]);
        assert_eq!(pairs("--append-system-prompt-file"), ["%ROOTS%/instructions/g/canvas-group.instructions.md"]);
        assert_eq!(pairs("--mcp-config"), ["%ROOTS%/agent-mcp/claude.json"]);
        assert!(argv.contains(&"--strict-mcp-config".to_owned()));
        let all = format!("{argv:?}{env:?}");
        assert!(!all.contains(r"C:\") && !env.iter().any(|(k, _)| k.starts_with('-')), "nothing of this machine: {all}");
    }
}
