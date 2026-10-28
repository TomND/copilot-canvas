//! Claude Code.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use canvas_common::protocol::{AgentLaunch, SessionSummary};

use super::claude_history::{ClaudeHistory, claude_dir, find_transcript, live_sessions, user_config};
use super::{AgentKind, Caps, Command, History, Integration, Provider};
use crate::index::is_session_id;

pub struct Claude;

/// Where Claude Code keeps its data (overridable for tests).
#[derive(Clone, Debug)]
pub struct ClaudeHome {
    pub dir: PathBuf,
    pub user_config: PathBuf,
}

impl Default for ClaudeHome {
    fn default() -> Self {
        Self { dir: claude_dir(), user_config: user_config() }
    }
}

/// True for the Claude desktop app's `claude.exe` (never a CLI).
fn is_desktop_app(path: &Path) -> bool {
    let lower = path.to_string_lossy().to_ascii_lowercase();
    ["\\windowsapps\\", "\\anthropicclaude\\", "\\programs\\claude\\", "\\claude desktop\\"].iter().any(|p| lower.contains(p))
}

/// The Claude Code CLI: `COPILOT_CANVAS_CLAUDE`, else `claude.exe` on PATH (not
/// the desktop app), an npm install's native binary, then `~/.local/bin`.
/// Batch scripts (`claude.cmd`) are never started (see [`super::ensure_not_batch`]).
pub fn find_cli() -> Result<String> {
    find_cli_in(std::env::var_os("COPILOT_CANVAS_CLAUDE"), std::env::var_os("PATH").unwrap_or_default(), &super::home())
}

fn find_cli_in(custom: Option<std::ffi::OsString>, path: std::ffi::OsString, home: &Path) -> Result<String> {
    if let Some(custom) = custom {
        let custom = custom.to_string_lossy().into_owned();
        super::ensure_not_batch(Path::new(&custom), "COPILOT_CANVAS_CLAUDE")?;
        return Ok(custom);
    }
    for dir in std::env::split_paths(&path) {
        let exe = dir.join("claude.exe");
        if exe.is_file() && !is_desktop_app(&exe) {
            return Ok(exe.display().to_string());
        }
        if dir.join("claude.cmd").is_file() {
            // The native binary behind npm's shim; the shim itself is never started.
            let native = dir.join("node_modules").join("@anthropic-ai").join("claude-code").join("bin").join("claude.exe");
            if native.is_file() {
                return Ok(native.display().to_string());
            }
        }
    }
    let local = home.join(".local").join("bin").join("claude.exe");
    if local.is_file() {
        return Ok(local.display().to_string());
    }
    anyhow::bail!("Claude Code (claude.exe) was not found on PATH (set COPILOT_CANVAS_CLAUDE to override)")
}

/// `--mcp-config` content: the user's MCP servers (`.claude.json`) minus `off`.
pub fn mcp_config(user_config: &Path, off: &[String]) -> Result<serde_json::Value> {
    let text = match std::fs::read_to_string(user_config) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", user_config.display())),
    };
    let mut servers = serde_json::Map::new();
    if !text.trim().is_empty() {
        let value: serde_json::Value = serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON", user_config.display()))?;
        if let Some(map) = value["mcpServers"].as_object() {
            for (name, entry) in map {
                if !off.iter().any(|o| o.eq_ignore_ascii_case(name)) {
                    servers.insert(name.clone(), entry.clone());
                }
            }
        }
    }
    Ok(serde_json::json!({ "mcpServers": servers }))
}

/// Folder of the per-session MCP configurations below the canvas home.
fn mcp_dir(home: &Path) -> PathBuf {
    home.join("mcp")
}

/// The canvas home: the plugin folder's parent, else `COPILOT_CANVAS_HOME`
/// from the integration environment.
fn canvas_home(integration: &Integration) -> Option<PathBuf> {
    integration.plugin_dir.as_ref().and_then(|p| p.parent()).map(Path::to_path_buf).or_else(|| {
        integration.env.iter().rev().find(|(k, v)| k == "COPILOT_CANVAS_HOME" && !v.is_empty()).map(|(_, v)| PathBuf::from(v))
    })
}

/// Removes the per-session files a launch wrote below the canvas `home`: the
/// MCP configuration `mcp/claude-<id>.json` (it holds the user's MCP server
/// settings, secrets included) and temporary files left by an interrupted
/// write. `id` is the Claude session id the launch used (`launch.native_id`,
/// else the terminal id). The service calls this when the terminal exits.
pub fn cleanup_session_files(home: &Path, id: &str) {
    if !is_session_id(id) {
        return;
    }
    let folder = mcp_dir(home);
    let _ = std::fs::remove_file(folder.join(format!("claude-{id}.json")));
    let temp = format!("claude-{id}.json.");
    if let Ok(entries) = std::fs::read_dir(&folder) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&temp) && name.ends_with(".tmp") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Writes `path` atomically (a temporary file in the same folder, renamed
/// over it) with a protected DACL that grants access to the current user only.
fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    let name = path.file_name().context("no file name")?.to_string_lossy().into_owned();
    let temp = path.with_file_name(format!("{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let written = create_private(&temp).and_then(|mut file| {
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)
    });
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written.with_context(|| format!("cannot write {}", path.display()))
}

/// Creates a new file that only the current user can open.
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::Storage::FileSystem::{CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL};

    let sid = crate::win::current_user_sid()?;
    let sddl: Vec<u16> = format!("D:P(A;;FA;;;{sid})").encode_utf16().chain(Some(0)).collect();
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated; the descriptor is freed below.
    if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut descriptor, std::ptr::null_mut()) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let attributes = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: descriptor, bInheritHandle: 0 };
    // SAFETY: `wide` is NUL-terminated and `attributes` outlives the call.
    let handle = unsafe { CreateFileW(wide.as_ptr(), GENERIC_WRITE, 0, &attributes, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, std::ptr::null_mut()) };
    let error = std::io::Error::last_os_error();
    // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
    unsafe { LocalFree(descriptor) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(error);
    }
    // SAFETY: a new, owned file handle.
    Ok(unsafe { std::fs::File::from_raw_handle(handle) })
}

impl Claude {
    /// The command for a launch against a given Claude Code home.
    pub fn command_in(&self, home: &ClaudeHome, id: &str, launch: &AgentLaunch, integration: &Integration) -> Result<Command> {
        let session = launch.native_id.as_deref().unwrap_or(id);
        anyhow::ensure!(is_session_id(session), "refusing to launch an invalid session id");
        let mut args: Vec<String> = Vec::new();
        let mut push = |items: &[&str]| args.extend(items.iter().map(|s| (*s).to_owned()));
        // The prompt is positional; it comes before the variadic options
        // (`--add-dir a b`) and must not look like an option itself.
        let positional = super::positional;
        if let Some(prompt) = launch.headless_prompt.as_ref() {
            // Passive run: one turn, never asks.
            push(&["-p", &positional(prompt)]);
            if launch.resume {
                push(&["-r", session]);
            } else {
                push(&["--session-id", session]);
            }
            push(&["--dangerously-skip-permissions"]);
            if let Some(folder) = &launch.group_folder {
                push(&["--add-dir", folder]);
            }
        } else {
            match launch.fork_from.as_deref() {
                Some(parent) => {
                    anyhow::ensure!(is_session_id(parent), "refusing to fork an invalid session id");
                    push(&["-r", parent, "--fork-session", "--session-id", session]);
                }
                None if launch.resume => push(&["-r", session]),
                None => push(&["--session-id", session]),
            }
            if !launch.resume
                && let Some(name) = launch.name.as_deref().filter(|n| !n.trim().is_empty())
            {
                push(&["-n", &positional(name.trim())]);
            }
            // A first prompt also applies to a forked or resumed session.
            if let Some(prompt) = launch.prompt.as_deref().filter(|p| !p.trim().is_empty()) {
                push(&[&positional(prompt)]);
            }
            // Permissions apply to every run (they are not saved with the session).
            if launch.allow_all {
                push(&["--dangerously-skip-permissions"]);
            }
        }
        // Nothing below is saved with a Claude session, so it applies to resumes too.
        for (name, value) in [("--model", &launch.model), ("--effort", &launch.effort), ("--agent", &launch.custom_agent)] {
            if let Some(value) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                args.extend([name.to_owned(), super::option_value(name, value)?.to_owned()]);
            }
        }
        for dir in launch.add_dirs.iter().map(|d| d.trim()).filter(|d| !d.is_empty()) {
            args.extend(["--add-dir".to_owned(), dir.to_owned()]);
        }
        if launch.headless_prompt.is_none()
            && let (Some(folder), Some(name)) = (&launch.group_folder, &launch.group_name)
        {
            let dirs = super::group_dirs(folder, name)?;
            for dir in [&dirs.ai_docs, &dirs.outputs] {
                args.extend(["--add-dir".to_owned(), dir.display().to_string()]);
            }
            args.extend(["--append-system-prompt-file".to_owned(), dirs.instructions_file.display().to_string()]);
        }
        if !launch.mcp_off.is_empty() {
            // Only the servers written to this file start (user scope minus the
            // ones turned off). It holds their settings (secrets included), so it
            // lives only below the canvas home, readable by the user alone, and
            // `cleanup_session_files` removes it when the terminal exits.
            match canvas_home(integration) {
                Some(canvas) => {
                    let config = mcp_config(&home.user_config, &launch.mcp_off)?;
                    let folder = mcp_dir(&canvas);
                    std::fs::create_dir_all(&folder).with_context(|| format!("cannot create {}", folder.display()))?;
                    let file = folder.join(format!("claude-{session}.json"));
                    write_private(&file, serde_json::to_string_pretty(&config)?.as_bytes())?;
                    args.extend(["--strict-mcp-config".to_owned(), "--mcp-config".to_owned(), file.display().to_string()]);
                }
                None => eprintln!("claude {}: no canvas home for the MCP configuration; all MCP servers stay on", super::short_id(session)),
            }
        }
        if let Some(plugin) = &integration.plugin_dir {
            args.extend(["--plugin-dir".to_owned(), plugin.display().to_string()]);
        }
        let mut env = integration.env.clone();
        // Claude Code reports turns with OSC 9;4 only outside Windows Terminal
        // and VS Code; the canvas terminal is neither.
        env.extend([("-WT_SESSION".to_owned(), String::new()), ("-TERM_PROGRAM".to_owned(), String::new()), ("ConEmuANSI".to_owned(), "ON".to_owned())]);
        env.extend(launch.env.iter().cloned());
        Ok(Command { program: self.program()?, args, env })
    }
}

impl Provider for Claude {
    fn kind(&self) -> AgentKind {
        AgentKind::Claude
    }

    fn caps(&self) -> Caps {
        Caps {
            preassigned_ids: true,
            native_fork: true,
            copy_fork: false,
            mcp_select: true,
            progress: true,
            config: true,
            automations: false,
            effort: true,
            modes: false,
        }
    }

    fn program(&self) -> Result<String> {
        find_cli()
    }

    fn is_session_id(&self, id: &str) -> bool {
        is_session_id(id)
    }

    fn reserved_args(&self) -> &'static [&'static str] {
        &["--session-id", "--resume", "-r", "--continue", "-c", "--fork-session", "--teleport", "--from-pr"]
    }

    fn command(&self, id: &str, launch: &AgentLaunch, integration: &Integration) -> Result<Command> {
        self.command_in(&ClaudeHome::default(), id, launch, integration)
    }

    fn history(&self) -> Box<dyn History> {
        Box::new(ClaudeHistory::new(claude_dir()))
    }

    fn is_live(&self, native_id: &str) -> bool {
        live_sessions(&claude_dir()).contains_key(native_id)
    }

    fn has_history(&self, native_id: &str) -> bool {
        is_session_id(native_id) && find_transcript(&claude_dir(), native_id, None).is_some()
    }

    fn history_root(&self) -> PathBuf {
        claude_dir().join("projects")
    }

    fn terminal_exited(&self, id: &str, home: &Path) {
        // The session's MCP choice (secrets included) is written again on every launch.
        cleanup_session_files(home, id);
    }
}

impl History for ClaudeHistory {
    fn poll(&mut self, tracked: &HashSet<String>, full_every: std::time::Duration) -> (Vec<SessionSummary>, Vec<String>) {
        ClaudeHistory::poll(self, tracked, full_every)
    }

    fn take_detected(&mut self) -> Vec<(String, String, serde_json::Value)> {
        ClaudeHistory::take_detected(self)
    }

    fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "12345678-1234-1234-1234-123456789abc";
    const PARENT: &str = "87654321-4321-4321-4321-cba987654321";

    fn setup() -> (ClaudeHome, Integration, PathBuf) {
        let root = std::env::temp_dir().join(format!("cc-claude-cmd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("home").join("plugin")).unwrap();
        let config = root.join(".claude.json");
        std::fs::write(
            &config,
            r#"{"numStartups":3,"mcpServers":{"github":{"type":"http","url":"https://api.example/mcp","headers":{"Authorization":"secret"}},"blender":{"type":"stdio","command":"uvx","args":["blender-mcp"],"env":{"K":"v"}}}}"#,
        )
        .unwrap();
        let home = ClaudeHome { dir: root.join(".claude"), user_config: config };
        let integration = Integration { plugin_dir: Some(root.join("home").join("plugin")), env: vec![("COPILOT_CANVAS_SESSION".into(), ID.into())] };
        // SAFETY: tests in this crate only read this variable.
        unsafe { std::env::set_var("COPILOT_CANVAS_CLAUDE", r"C:\bin\claude.exe") };
        (home, integration, root)
    }

    #[test]
    fn new_resume_fork_and_headless_command_lines() {
        let (home, integration, root) = setup();
        let plugin = integration.plugin_dir.clone().unwrap().display().to_string();
        let new = AgentLaunch { agent: "claude".into(), name: Some("Fix it".into()), prompt: Some("hello".into()), model: Some("opus".into()), effort: Some("high".into()), ..Default::default() };
        let cmd = Claude.command_in(&home, ID, &new, &integration).unwrap();
        assert_eq!(cmd.program, r"C:\bin\claude.exe");
        assert_eq!(cmd.args, ["--session-id", ID, "-n", "Fix it", "hello", "--model", "opus", "--effort", "high", "--plugin-dir", &plugin]);
        assert!(cmd.env.contains(&("-WT_SESSION".into(), String::new())) && cmd.env.contains(&("ConEmuANSI".into(), "ON".into())));
        assert_eq!(cmd.env[0], ("COPILOT_CANVAS_SESSION".into(), ID.into()));
        let dashed = AgentLaunch { prompt: Some("-v please".into()), ..new.clone() };
        assert_eq!(Claude.command_in(&home, ID, &dashed, &integration).unwrap().args[4], " -v please", "never parsed as an option");

        let resume = AgentLaunch { resume: true, allow_all: true, model: None, effort: None, prompt: None, ..new.clone() };
        assert_eq!(Claude.command_in(&home, ID, &resume, &integration).unwrap().args, ["-r", ID, "--dangerously-skip-permissions", "--plugin-dir", &plugin]);

        let fork = AgentLaunch { fork_from: Some(PARENT.into()), model: None, effort: None, ..new.clone() };
        assert_eq!(Claude.command_in(&home, ID, &fork, &integration).unwrap().args[..8], ["-r", PARENT, "--fork-session", "--session-id", ID, "-n", "Fix it", "hello"]);
        assert!(Claude.command_in(&home, ID, &AgentLaunch { fork_from: Some("nope".into()), ..new.clone() }, &integration).is_err());

        let passive = AgentLaunch { headless_prompt: Some("run".into()), group_folder: Some(r"C:\g".into()), resume: true, ..AgentLaunch::default() };
        assert_eq!(Claude.command_in(&home, ID, &passive, &integration).unwrap().args, ["-p", "run", "-r", ID, "--dangerously-skip-permissions", "--add-dir", r"C:\g", "--plugin-dir", &plugin]);
        let first_run = AgentLaunch { resume: false, ..passive };
        assert_eq!(Claude.command_in(&home, ID, &first_run, &integration).unwrap().args[2..4], ["--session-id", ID]);
        assert!(Claude.command_in(&home, "not-a-uuid", &new, &integration).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mcp_off_writes_a_strict_config_without_those_servers() {
        let (home, integration, root) = setup();
        let launch = AgentLaunch { agent: "claude".into(), mcp_off: vec!["GitHub".into()], add_dirs: vec![r"C:\extra".into()], ..Default::default() };
        let args = Claude.command_in(&home, ID, &launch, &integration).unwrap().args;
        let at = args.iter().position(|a| a == "--strict-mcp-config").expect("strict");
        assert_eq!(args[at + 1], "--mcp-config");
        let written: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&args[at + 2]).unwrap()).unwrap();
        assert!(args[at + 2].starts_with(&root.join("home").join("mcp").display().to_string()));
        let servers = written["mcpServers"].as_object().unwrap();
        assert_eq!(servers.keys().collect::<Vec<_>>(), ["blender"]);
        assert_eq!(servers["blender"]["env"]["K"], "v", "the remaining servers keep their configuration");
        assert_eq!(args[2..4], ["--add-dir", r"C:\extra"]);
        // No servers turned off: Claude's own MCP configuration applies.
        let plain = AgentLaunch { mcp_off: Vec::new(), ..launch.clone() };
        assert!(!Claude.command_in(&home, ID, &plain, &integration).unwrap().args.iter().any(|a| a.contains("mcp")));
        assert_eq!(mcp_config(&root.join("missing.json"), &[]).unwrap(), serde_json::json!({ "mcpServers": {} }));
        // The file goes away with the session, temporary leftovers too.
        let file = PathBuf::from(&args[at + 2]);
        let leftover = file.with_file_name(format!("claude-{ID}.json.0123.tmp"));
        std::fs::write(&leftover, "x").unwrap();
        cleanup_session_files(&root.join("home"), "not-a-session");
        assert!(file.is_file(), "invalid ids touch nothing");
        cleanup_session_files(&root.join("home"), ID);
        assert!(!file.exists() && !leftover.exists());
        // The service removes it when the session's terminal exits.
        Claude.command_in(&home, ID, &launch, &integration).unwrap();
        assert!(file.is_file());
        Claude.terminal_exited(ID, &root.join("home"));
        assert!(!file.exists(), "the per-session file is removed when its terminal exits");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_mcp_file_never_falls_back_to_temp() {
        let (home, _, root) = setup();
        let launch = AgentLaunch { agent: "claude".into(), mcp_off: vec!["github".into()], ..Default::default() };
        let homeless = Integration { plugin_dir: None, env: Vec::new() };
        let args = Claude.command_in(&home, ID, &launch, &homeless).unwrap().args;
        assert!(!args.iter().any(|a| a.contains("mcp")), "no canvas home: the selection is refused, not written to %TEMP%");
        // COPILOT_CANVAS_HOME names the home when the plugin folder is missing.
        let canvas = root.join("canvas");
        let env_only = Integration { plugin_dir: None, env: vec![("COPILOT_CANVAS_HOME".into(), canvas.display().to_string())] };
        let args = Claude.command_in(&home, ID, &launch, &env_only).unwrap().args;
        let file = canvas.join("mcp").join(format!("claude-{ID}.json"));
        assert_eq!(args.last().unwrap(), &file.display().to_string());
        let written: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert!(written["mcpServers"]["blender"].is_object());
        // Rewriting replaces the file atomically and leaves no temporary files.
        Claude.command_in(&home, ID, &launch, &env_only).unwrap();
        assert_eq!(std::fs::read_dir(canvas.join("mcp")).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn private_files_grant_only_the_current_user() {
        let dir = std::env::temp_dir().join(format!("cc-private-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("secret.json");
        write_private(&file, b"{}").unwrap();
        write_private(&file, b"{\"a\":1}").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{\"a\":1}");
        let run = |program: &str, arg: &Path| String::from_utf8_lossy(&std::process::Command::new(program).arg(arg).output().unwrap().stdout).to_ascii_lowercase();
        let acl = run("icacls", &file).replace(&file.display().to_string().to_ascii_lowercase(), "");
        let user = run("whoami", Path::new("/user")).lines().filter_map(|l| l.split_whitespace().next()).find(|w| w.contains('\\')).unwrap().to_owned();
        let entries: Vec<&str> = acl.lines().map(str::trim).filter(|l| l.contains(":(")).collect();
        assert_eq!(entries, [format!("{user}:(f)")], "{acl}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn batch_shims_are_never_started() {
        let root = std::env::temp_dir().join(format!("cc-claude-cli-{}", uuid::Uuid::new_v4()));
        let bin = root.join("npm");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("claude.cmd"), "@echo off").unwrap();
        let path = std::env::join_paths([&bin]).unwrap();
        let error = find_cli_in(None, path.clone(), &root).unwrap_err().to_string();
        assert!(error.contains("COPILOT_CANVAS_CLAUDE"), "{error}");
        let native = bin.join("node_modules").join("@anthropic-ai").join("claude-code").join("bin");
        std::fs::create_dir_all(&native).unwrap();
        std::fs::write(native.join("claude.exe"), "").unwrap();
        assert_eq!(find_cli_in(None, path.clone(), &root).unwrap(), native.join("claude.exe").display().to_string());
        for script in [r"C:\x\claude.cmd", r"C:\x\CLAUDE.BAT"] {
            let error = find_cli_in(Some(script.into()), path.clone(), &root).unwrap_err().to_string();
            assert!(error.contains("batch script") && error.contains("COPILOT_CANVAS_CLAUDE"), "{error}");
        }
        assert_eq!(find_cli_in(Some(r"C:\x\claude.exe".into()), path, &root).unwrap(), r"C:\x\claude.exe");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn names_and_option_values_cannot_inject_options() {
        let (home, integration, root) = setup();
        let launch = AgentLaunch { agent: "claude".into(), name: Some("-x".into()), prompt: Some("--help".into()), ..Default::default() };
        let args = Claude.command_in(&home, ID, &launch, &integration).unwrap().args;
        assert_eq!(args[..5], ["--session-id", ID, "-n", " -x", " --help"]);
        for bad in [AgentLaunch { model: Some("-x".into()), ..launch.clone() }, AgentLaunch { effort: Some("--evil".into()), ..launch.clone() }, AgentLaunch { custom_agent: Some("a\nb".into()), ..launch }] {
            assert!(Claude.command_in(&home, ID, &bad, &integration).is_err());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn group_folders_add_dirs_and_instructions() {
        let (home, integration, root) = setup();
        let folder = root.join("group");
        let launch = AgentLaunch { agent: "claude".into(), group_folder: Some(folder.display().to_string()), group_name: Some("Team".into()), ..Default::default() };
        let args = Claude.command_in(&home, ID, &launch, &integration).unwrap().args;
        let docs = folder.join(canvas_model::doc::AI_DOCS).display().to_string();
        let outputs = folder.join(canvas_model::doc::HUMAN_OUTPUTS).display().to_string();
        let instructions = folder.join(".canvas").join("canvas-group.instructions.md");
        assert_eq!(args[2..8], ["--add-dir", &docs, "--add-dir", &outputs, "--append-system-prompt-file", &instructions.display().to_string()]);
        assert!(std::fs::read_to_string(instructions).unwrap().contains("Team"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_desktop_app_is_never_the_cli() {
        assert!(is_desktop_app(Path::new(r"C:\Program Files\WindowsApps\Claude_4.29.0.0_x64__pzs8sxrjxfjjc\app\claude.exe")));
        assert!(is_desktop_app(Path::new(r"C:\Users\me\AppData\Local\Microsoft\WindowsApps\claude.exe")));
        assert!(is_desktop_app(Path::new(r"C:\Users\me\AppData\Local\AnthropicClaude\app-1.0\claude.exe")));
        assert!(!is_desktop_app(Path::new(r"C:\Users\me\.local\bin\claude.exe")));
    }
}
