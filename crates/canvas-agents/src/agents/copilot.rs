//! GitHub Copilot CLI.

use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::Result;
use canvas_common::protocol::{AgentLaunch, SessionSummary};

use super::{AgentKind, Caps, Command, History, Integration, Provider};
use crate::index::{Index, is_session_id};

pub struct Copilot;

static SESSIONS_DIR: OnceLock<PathBuf> = OnceLock::new();

/// The service's history root (`--sessions-dir`); set once at start.
pub fn set_sessions_dir(path: PathBuf) {
    let _ = SESSIONS_DIR.set(path);
}

/// Copilot's session history: `--sessions-dir`, else `~/.copilot/session-state`.
pub fn sessions_dir() -> PathBuf {
    SESSIONS_DIR.get().cloned().unwrap_or_else(|| super::home().join(".copilot").join("session-state"))
}

impl Provider for Copilot {
    fn kind(&self) -> AgentKind {
        AgentKind::Copilot
    }

    fn caps(&self) -> Caps {
        AgentKind::Copilot.caps()
    }

    fn program(&self) -> Result<String> {
        super::find_program("COPILOT_CANVAS_COPILOT", &["copilot.exe"])
    }

    fn is_session_id(&self, id: &str) -> bool {
        is_session_id(id)
    }

    fn reserved_args(&self) -> &'static [&'static str] {
        &["--session-id", "--resume", "-r", "--continue"]
    }

    fn command(&self, id: &str, launch: &AgentLaunch, integration: &Integration) -> Result<Command> {
        let session = launch.native_id.as_deref().unwrap_or(id);
        anyhow::ensure!(is_session_id(session), "refusing to launch an invalid session id");
        // No --mouse flag: Copilot saves it to the user's global settings.
        let mut args = vec!["--session-id".to_owned(), session.to_owned()];
        if !launch.resume && let Some(name) = launch.name.as_ref().filter(|n| !n.trim().is_empty()) {
            args.extend(["--name".to_owned(), name.clone()]);
        }
        let mut env = Vec::new();
        if let Some(prompt) = launch.headless_prompt.as_ref() {
            // Passive run: one turn, never asks.
            args.extend(["-p".to_owned(), prompt.clone(), "--allow-all".to_owned(), "--no-ask-user".to_owned()]);
            if let Some(folder) = &launch.group_folder {
                args.extend(["--add-dir".to_owned(), folder.clone()]);
            }
        } else {
            // A first prompt also applies to a forked subsession (resumed from its copy).
            if let Some(prompt) = launch.prompt.as_ref().filter(|p| !p.trim().is_empty()) {
                args.extend(["-i".to_owned(), prompt.clone()]);
            }
            // Permissions apply to every run (they are not saved with the session).
            if launch.allow_all {
                args.push("--allow-all".to_owned());
            }
            if !launch.resume {
                let mut flag = |name: &str, value: &Option<String>| {
                    if let Some(value) = value.as_ref().filter(|v| !v.trim().is_empty()) {
                        args.push(name.to_owned());
                        args.push(value.trim().to_owned());
                    }
                };
                flag("--model", &launch.model);
                flag("--reasoning-effort", &launch.effort);
                flag("--mode", &launch.mode);
                flag("--agent", &launch.custom_agent);
                flag("--context", &launch.context);
                for dir in launch.add_dirs.iter().filter(|d| !d.trim().is_empty()) {
                    args.extend(["--add-dir".to_owned(), dir.trim().to_owned()]);
                }
            }
        }
        // The session's MCP server choice applies to every run (resumes too).
        args.extend(canvas_model::doc::mcp_off_args(&launch.mcp_off));
        if let Some(plugin) = &integration.plugin_dir {
            args.extend(["--plugin-dir".to_owned(), plugin.display().to_string()]);
        }
        env.extend(integration.env.iter().cloned());
        if launch.headless_prompt.is_none()
            && let (Some(folder), Some(name)) = (&launch.group_folder, &launch.group_name)
        {
            let dirs = super::group_dirs(folder, name)?;
            for dir in [&dirs.ai_docs, &dirs.outputs] {
                args.extend(["--add-dir".to_owned(), dir.display().to_string()]);
            }
            let mut list = std::env::var("COPILOT_CUSTOM_INSTRUCTIONS_DIRS").unwrap_or_default();
            if !list.is_empty() {
                list.push(',');
            }
            list.push_str(&dirs.instructions.display().to_string());
            env.push(("COPILOT_CUSTOM_INSTRUCTIONS_DIRS".to_owned(), list));
        }
        env.extend(launch.env.iter().cloned());
        Ok(Command { program: self.program()?, args, env })
    }

    fn history(&self) -> Box<dyn History> {
        Box::new(Index::new(sessions_dir()))
    }

    fn is_live(&self, native_id: &str) -> bool {
        crate::service::session_live(&sessions_dir(), native_id)
    }

    fn has_history(&self, native_id: &str) -> bool {
        is_session_id(native_id) && sessions_dir().join(native_id).join("events.jsonl").is_file()
    }

    fn history_root(&self) -> PathBuf {
        sessions_dir()
    }
}

impl History for Index {
    fn poll(&mut self, tracked: &std::collections::HashSet<String>, full_every: std::time::Duration) -> (Vec<SessionSummary>, Vec<String>) {
        Index::poll(self, tracked, full_every)
    }

    fn take_detected(&mut self) -> Vec<(String, String, serde_json::Value)> {
        Index::take_detected(self)
    }

    fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_resume_and_headless_command_lines_match_copilot_flags() {
        let id = "12345678-1234-1234-1234-123456789abc";
        let integration = Integration { plugin_dir: Some(PathBuf::from(r"C:\h\plugin")), env: vec![("COPILOT_CANVAS_SESSION".into(), id.into())] };
        // SAFETY: tests in this module are the only readers of this variable.
        unsafe { std::env::set_var("COPILOT_CANVAS_COPILOT", r"C:\bin\copilot.exe") };
        let new = AgentLaunch {
            agent: "copilot".into(),
            name: Some("Fix it".into()),
            prompt: Some("hello".into()),
            model: Some("gpt-x".into()),
            allow_all: true,
            mcp_off: vec!["github".into()],
            ..Default::default()
        };
        let cmd = Copilot.command(id, &new, &integration).unwrap();
        assert_eq!(cmd.program, r"C:\bin\copilot.exe");
        assert_eq!(
            cmd.args,
            [
                "--session-id", id, "--name", "Fix it", "-i", "hello", "--allow-all", "--model", "gpt-x", "--disable-mcp-server", "github",
                "--plugin-dir", r"C:\h\plugin"
            ]
        );
        let resume = AgentLaunch { resume: true, model: Some("ignored on resume".into()), ..new.clone() };
        let args = Copilot.command(id, &resume, &integration).unwrap().args;
        assert!(!args.iter().any(|a| a == "--name" || a == "--model"), "{args:?}");
        let passive = AgentLaunch { headless_prompt: Some("run".into()), group_folder: Some(r"C:\g".into()), ..resume };
        let args = Copilot.command(id, &passive, &integration).unwrap().args;
        assert_eq!(args[2..8], ["-p", "run", "--allow-all", "--no-ask-user", "--add-dir", r"C:\g"]);
        assert!(Copilot.command("not-a-uuid", &new, &integration).is_err());
    }
}
