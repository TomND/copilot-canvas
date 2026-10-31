//! Windows: `ShellExecuteW` for URLs, File Explorer for folders and files.

use std::path::Path;

use anyhow::{Context, Result};

/// Hands a URL to its registered handler (never runs it as a program).
pub fn url(target: &str) -> Result<()> {
    use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx};
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (verb, file) = (wide("open"), wide(target));
    // SAFETY: null-terminated UTF-16 strings that outlive the call.
    let result = unsafe {
        CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32);
        ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL)
    };
    anyhow::ensure!(result as usize > 32, "Windows could not open {target}");
    Ok(())
}

/// Opens a folder in File Explorer.
pub fn folder(path: &Path) -> Result<()> {
    std::process::Command::new("explorer.exe").arg(path).spawn().context("cannot open File Explorer")?;
    Ok(())
}

/// Shows a file selected in its folder.
pub fn reveal(path: &Path) -> Result<()> {
    std::process::Command::new("explorer.exe").arg(format!("/select,{}", path.display())).spawn().context("cannot open File Explorer")?;
    Ok(())
}

/// Opens a file or URL with its default program (Explorer decides).
pub fn default_app(target: &str) -> Result<()> {
    std::process::Command::new("explorer.exe").arg(target).spawn().map(|_| ()).map_err(|e| anyhow::anyhow!("cannot open {target}: {e}"))
}

/// VS Code's `vscode://` handler is registered on this machine.
pub fn vscode_installed() -> bool {
    use std::sync::OnceLock;
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    *INSTALLED.get_or_init(|| {
        std::process::Command::new("reg.exe")
            .args(["query", r"HKCR\vscode\shell\open\command"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// Opens a file (at a line) in VS Code through its `vscode://` handler.
pub fn vscode_file(path: &Path, line: Option<u32>) -> Result<()> {
    url(&super::vscode_file_url(path, line))
}

/// Starts a console program in a console window of its own (the TUI from the GUI).
pub fn in_terminal(exe: &Path, args: &[String]) -> Result<()> {
    use crate::spawn::{CommandExt, NEW_CONSOLE, NEW_PROCESS_GROUP};
    std::process::Command::new(exe)
        .args(args)
        .stdin(std::process::Stdio::null())
        .spawn_flags(NEW_PROCESS_GROUP | NEW_CONSOLE)
        .spawn()
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("cannot start {}: {e}", exe.display()))
}
