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
pub fn guest_command(kind: AgentKind, id: &str, launch: &AgentLaunch) -> Result<crate::agents::ArgsEnv> {
    let mut launch = launch.clone();
    launch.group_folder = None;
    launch.group_name = None;
    launch.add_dirs.clear();
    launch.headless_prompt = None;
    anyhow::ensure!(launch.fork_from.is_none(), "forking a session that runs on a VM is not available");
    let integration = crate::agents::Integration::default();
    let (args, mut env) = match kind {
        AgentKind::Copilot => crate::agents::copilot::args(id, &launch, &integration)?,
        other => bail!("{} sessions cannot run on a VM yet (Copilot CLI can)", other.label()),
    };
    // The canvas's own variables are set by the guest's side.
    env.retain(|(k, _)| !canvas_common::env::is_ours(k));
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

/// `atelier vm attach --box ID`: claims a VM for the box when it has none
/// (the pack's `claim` hook), then runs the `shell-command` hook's command
/// in this terminal (the agent in the guest). Its steps are printed.
pub fn attach(home: &Path, id: &str) -> Result<()> {
    use std::io::Write;
    let folder = super::pack::configured(home).context("no VM pack is configured: VM sessions are off")?;
    let (pack, status) = super::pack::load(&folder);
    anyhow::ensure!(status.errors() == 0, "the VM pack has {} error(s): see `atelier vm pack check`", status.errors());
    let hooks = super::hooks::Hooks::of(&pack);
    let mut intent = load(home, id)?.with_context(|| format!("no VM launch for box {id}"))?;
    let say = |text: &str| {
        let mut out = std::io::stdout();
        let _ = write!(out, "\x1b[2m☁ {text}\x1b[0m\r\n");
        let _ = out.flush();
    };
    if intent.sandbox.is_empty() {
        let repo = pack.repo(&intent.placement.repo).with_context(|| format!("the VM pack has no repository {:?}", intent.placement.repo))?;
        let size = pack.size(intent.placement.size.as_deref()).context("the VM pack has no such size")?;
        let branch = intent.placement.branch.clone().unwrap_or_else(|| canvas_model::vm::branch_for_session(&intent.r#box));
        anyhow::ensure!(canvas_model::vm::branch_ok(&branch), "{branch:?} is not a branch name");
        say(&format!("Claiming a VM for {} on {branch} ({})…", repo.key, size.name));
        intent.state = VmState::Claiming;
        save(home, &mut intent)?;
        let answer = hooks.call(
            "claim",
            &serde_json::json!({
                "box": intent.r#box, "repo": repo.key, "url": repo.url, "branch": branch, "base": repo.default_branch,
                "size": size.name, "cpu": format!("{}m", size.cpu_milli), "memory": format!("{}Mi", size.memory_mi),
                "labels": { "atelier.box": intent.r#box, "atelier.repo": repo.key },
            }),
        );
        match answer {
            Ok(answer) => {
                intent.sandbox = answer["sandbox"].as_str().unwrap_or_default().to_owned();
                intent.cwd = answer["cwd"].as_str().unwrap_or_default().to_owned();
                intent.placement.branch = Some(branch);
                anyhow::ensure!(!intent.sandbox.is_empty(), "the claim hook named no sandbox");
            }
            Err(error) => {
                intent.state = VmState::Failed;
                intent.error = format!("{error:#}");
                save(home, &mut intent)?;
                return Err(error);
            }
        }
    } else {
        say(&format!("Joining the VM of this session ({})…", intent.sandbox));
    }
    let answer = hooks.call("shell-command", &serde_json::json!({ "sandbox": intent.sandbox, "box": intent.r#box, "argv": intent.argv, "env": intent.env.iter().cloned().collect::<std::collections::BTreeMap<_, _>>(), "cwd": intent.cwd }))?;
    let argv: Vec<String> = answer["argv"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
    anyhow::ensure!(!argv.is_empty(), "the shell-command hook gave no command");
    intent.state = VmState::Running;
    intent.error.clear();
    save(home, &mut intent)?;
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
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
        let (argv, env) = guest_command(AgentKind::Copilot, id, &launch).unwrap();
        assert_eq!(argv[..3], ["copilot", "--session-id", id]);
        assert!(argv.windows(2).any(|w| w == ["-i", "go"]) && argv.contains(&"--allow-all".to_owned()));
        let all = format!("{argv:?}{env:?}");
        assert!(!all.contains(r"C:\") && !all.contains("--plugin-dir") && !all.contains("--add-dir") && !all.contains("ATELIER_HOME"), "{all}");
        assert!(env.contains(&("KEEP".into(), "1".into())));
        assert!(guest_command(AgentKind::Claude, id, &launch).is_err(), "Copilot first");
        assert!(guest_command(AgentKind::Copilot, id, &AgentLaunch { fork_from: Some("p".into()), ..launch }).is_err(), "no forks of VM sessions");
        let env = guest_env(id, AgentKind::Copilot);
        assert!(env.contains(&("ATELIER_SESSION".into(), id.into())) && env.contains(&("ATELIER_REMOTE".into(), "satellite-lite".into())));
    }
}
