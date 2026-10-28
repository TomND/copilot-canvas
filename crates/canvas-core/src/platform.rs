//! What a front end provides from the machine it runs on: the clipboard,
//! opening links, bells, taskbar progress and the window title. The TUI's
//! implementation ([`TerminalPlatform`]) writes bells and progress to the
//! outer terminal; the GUI uses the Windows shell directly. In remote mode
//! all of it stays on the client's machine (PLAN.md §6): text and OSC 52
//! copies use this clipboard, bells and progress this taskbar, URLs this
//! browser; only images are copied to the service's clipboard for a paste.

use anyhow::Result;

pub trait Platform: Send + Sync {
    fn set_clipboard_text(&self, text: &str) -> Result<()> {
        crate::clipboard::set_text(text)
    }

    fn clipboard_text(&self) -> Result<String> {
        crate::clipboard::get_text()
    }

    /// The clipboard holds text (an image or copied files do not).
    fn clipboard_has_text(&self) -> bool {
        crate::clipboard::has_text()
    }

    fn clipboard_has_image(&self) -> bool {
        crate::clipboard::has_image()
    }

    /// The clipboard's image as PNG (sent to a remote service's clipboard).
    fn clipboard_image_png(&self) -> Result<Vec<u8>> {
        crate::clipboard::get_image_png()
    }

    /// Opens a URL or path safely (never executes it); returns a status line.
    fn open_link(&self, target: &str) -> Result<String> {
        crate::links::open(target)
    }

    /// Opens a `vscode://` URI (a remote file through a VS Code tunnel).
    fn open_uri(&self, uri: &str) -> Result<()> {
        crate::links::open_vscode_uri(uri)
    }

    /// A session rang its bell or sent a notification. `out` is the byte
    /// stream for the outer terminal (written before the next frame).
    fn bell(&self, out: &mut Vec<u8>) {
        out.push(0x07);
    }

    /// Mirrors the canvas state to the taskbar: 0 none, 3 busy, 4 paused
    /// (needs you); `value` in percent.
    fn progress(&self, state: u8, value: u8, out: &mut Vec<u8>) {
        out.extend_from_slice(format!("\x1b]9;4;{state};{value}\x07").as_bytes());
    }

    /// The window title (`attention`: sessions that need you).
    fn title(&self, _attention: usize) {}

    /// A session's program sent a desktop notification (OSC 777 / OSC 9;
    /// GUI: a toast). The bell for it is rung separately.
    fn notification(&self, _session: &str, _title: &str, _body: &str) {}

    /// A session started to need you (it waits for an answer or a
    /// permission; GUI: a toast and a flashing taskbar button when the
    /// window is in the background).
    fn needs_you(&self, _session: &str) {}

    /// Opens the other front end (TUI from the GUI, GUI from the TUI) on the
    /// same service and canvas with `args` (PLAN Phase 7: the TUI stays one
    /// keystroke away). Returns a status line.
    fn open_other_client(&self, args: &[String]) -> Result<String> {
        let gui = build_sibling("copilot-canvas-gui.exe")?;
        spawn_client(&gui, args, false)?;
        Ok("Opened the GPU client on this canvas.".into())
    }
}

/// `name` next to the running program (or the build that runs the service).
pub fn build_sibling(name: &str) -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?.with_file_name(name);
    if exe.is_file() {
        return Ok(exe);
    }
    let fallback = crate::paths::build_exe()?.with_file_name(name);
    anyhow::ensure!(fallback.is_file(), "{name} is not next to this program");
    Ok(fallback)
}

/// `COPILOT_CANVAS_OTHER_CLIENT_DRYRUN=FILE`: where a test wants the command
/// line of a client this one would start. Only honoured together with
/// `COPILOT_CANVAS_SELFTEST=1` (the self-tests set both), so a stray
/// variable never stops a real client from starting.
pub fn other_client_dryrun() -> Option<std::ffi::OsString> {
    let selftest = std::env::var("COPILOT_CANVAS_SELFTEST").is_ok_and(|v| v.trim() == "1");
    std::env::var_os("COPILOT_CANVAS_OTHER_CLIENT_DRYRUN").filter(|file| selftest && !file.is_empty())
}

/// Starts a front end detached from this one (`console`: in a new console
/// window, for the TUI). Under a self-test ([`other_client_dryrun`]) the
/// command line is written to a file instead.
pub fn spawn_client(exe: &std::path::Path, args: &[String], console: bool) -> Result<()> {
    use std::os::windows::process::CommandExt;
    if let Some(file) = other_client_dryrun() {
        let line = std::iter::once(exe.display().to_string()).chain(args.iter().cloned()).collect::<Vec<_>>().join("\u{1f}");
        std::fs::write(file, line)?;
        return Ok(());
    }
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let flags = CREATE_NEW_PROCESS_GROUP | if console { CREATE_NEW_CONSOLE } else { DETACHED_PROCESS };
    std::process::Command::new(exe)
        .args(args)
        .stdin(std::process::Stdio::null())
        .creation_flags(flags)
        .spawn()
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("cannot start {}: {e}", exe.display()))
}

/// The TUI: bells and OSC 9;4 progress go to the outer terminal.
pub struct TerminalPlatform;

impl Platform for TerminalPlatform {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_terminal_platform_writes_bells_and_progress_for_the_outer_terminal() {
        let mut out = Vec::new();
        TerminalPlatform.bell(&mut out);
        TerminalPlatform.progress(4, 100, &mut out);
        assert_eq!(out, b"\x07\x1b]9;4;4;100\x07");
    }
}
