//! macOS (`/usr/bin/open`, LaunchServices) and Linux (`xdg-open`, the
//! freedesktop FileManager1 interface).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

fn macos() -> bool {
    crate::host::platform() == crate::host::Platform::MacOs
}

/// Runs an opener without waiting for it (reaped by a thread).
fn spawn(program: &str, args: &[&std::ffi::OsStr]) -> Result<()> {
    let mut child = Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().with_context(|| format!("cannot run {program}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn opener() -> &'static str {
    if macos() { "/usr/bin/open" } else { "xdg-open" }
}

/// Hands a URL to its registered handler.
pub fn url(target: &str) -> Result<()> {
    anyhow::ensure!(!target.starts_with('-'), "not a URL: {target}");
    spawn(opener(), &[target.as_ref()])
}

/// Whether opening `path` could run something (an executable file, an app
/// bundle, a desktop entry or a script for Terminal).
fn runnable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
    if ["app", "desktop", "command", "tool", "terminal", "workflow", "pkg", "mpkg", "appimage"].contains(&ext.as_str()) {
        return true;
    }
    std::fs::metadata(path).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(true)
}

/// Opens a folder in the file manager (app bundles are revealed, never launched).
pub fn folder(path: &Path) -> Result<()> {
    anyhow::ensure!(path.is_dir(), "{} is not a folder", path.display());
    if macos() && runnable(path) {
        return reveal(path);
    }
    spawn(opener(), &[path.as_os_str()])
}

/// Shows a file selected in its folder.
pub fn reveal(path: &Path) -> Result<()> {
    if macos() {
        return spawn("/usr/bin/open", &["-R".as_ref(), path.as_os_str()]);
    }
    let uri = format!("file://{}", percent(path));
    let shown = Command::new("dbus-send")
        .args(["--session", "--print-reply", "--dest=org.freedesktop.FileManager1", "/org/freedesktop/FileManager1", "org.freedesktop.FileManager1.ShowItems"])
        .arg(format!("array:string:{uri}"))
        .arg("string:")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if shown {
        return Ok(());
    }
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"));
    spawn("xdg-open", &[parent.as_os_str()])
}

fn percent(path: &Path) -> String {
    path.display().to_string().bytes().map(|b| if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// Opens a file with its default program, or a URL; files that could run
/// are revealed instead.
pub fn default_app(target: &str) -> Result<()> {
    if target.starts_with("http://") || target.starts_with("https://") {
        return url(target);
    }
    let path = Path::new(target);
    if path.is_dir() {
        return folder(path);
    }
    if runnable(path) {
        return reveal(path);
    }
    spawn(opener(), &[path.as_os_str()])
}

/// `code` on `PATH` (or in the usual install folders).
fn code_cli() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).map(|d| d.join("code")).collect()).unwrap_or_default();
    candidates.push(PathBuf::from("/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code"));
    candidates.push(PathBuf::from("/usr/local/bin/code"));
    candidates.push(PathBuf::from("/snap/bin/code"));
    candidates.into_iter().find(|c| c.is_file())
}

/// VS Code is installed: its `code` command, or (Linux) a `vscode` URL handler.
pub fn vscode_installed() -> bool {
    use std::sync::OnceLock;
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    *INSTALLED.get_or_init(|| {
        code_cli().is_some()
            || (!macos()
                && Command::new("xdg-mime")
                    .args(["query", "default", "x-scheme-handler/vscode"])
                    .stderr(Stdio::null())
                    .output()
                    .is_ok_and(|out| out.status.success() && !String::from_utf8_lossy(&out.stdout).trim().is_empty()))
    })
}

/// Opens a file (at a line) in VS Code: `code -g FILE:LINE`, else its URL handler.
pub fn vscode_file(path: &Path, line: Option<u32>) -> Result<()> {
    if let Some(code) = code_cli() {
        let mut target = path.as_os_str().to_owned();
        if let Some(line) = line {
            target.push(format!(":{line}"));
        }
        return spawn(&code.to_string_lossy(), &["-g".as_ref(), target.as_os_str()]);
    }
    url(&super::vscode_file_url(path, line))
}

/// Quotes a word for a POSIX shell.
fn shell_quote(word: &str) -> String {
    if !word.is_empty() && word.bytes().all(|b| b.is_ascii_alphanumeric() || b"/-_.,:=+@%".contains(&b)) {
        return word.to_owned();
    }
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Starts a console program in a terminal window of its own (the TUI from
/// the GUI). macOS: a `.command` file opened in Terminal (or the terminal
/// app named by `COPILOT_CANVAS_TERMINAL`); no Apple Events permission is
/// needed. Linux: `$TERMINAL`, then the usual terminal emulators.
pub fn in_terminal(exe: &Path, args: &[String]) -> Result<()> {
    let line = std::iter::once(exe.display().to_string()).chain(args.iter().cloned()).map(|w| shell_quote(&w)).collect::<Vec<_>>().join(" ");
    if macos() {
        let dir = std::env::temp_dir().join(format!("copilot-canvas-{}", crate::host::user_name()));
        crate::fs::private_dir(&dir)?;
        let script = dir.join(format!("open-{}.command", canvas_common::paths::hex(&canvas_common::protocol::nonce()[..6])));
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o700).open(&script)?;
            writeln!(file, "#!/bin/sh\nrm -f {}\nexec {line}", shell_quote(&script.to_string_lossy()))?;
        }
        let app = std::env::var("COPILOT_CANVAS_TERMINAL").ok().filter(|a| !a.trim().is_empty()).unwrap_or_else(|| "Terminal".into());
        return spawn("/usr/bin/open", &["-a".as_ref(), app.as_ref(), script.as_os_str()]);
    }
    let shell_line = ["sh", "-c", &line];
    let mut tries: Vec<Vec<String>> = Vec::new();
    if let Ok(terminal) = std::env::var("TERMINAL")
        && !terminal.trim().is_empty()
    {
        tries.push([terminal.as_str(), "-e"].iter().chain(shell_line.iter()).map(|s| s.to_string()).collect());
    }
    for prefix in [&["x-terminal-emulator", "-e"][..], &["gnome-terminal", "--"], &["ptyxis", "--"], &["konsole", "-e"], &["kitty"], &["alacritty", "-e"], &["foot"], &["wezterm", "start", "--"], &["xterm", "-e"]] {
        tries.push(prefix.iter().chain(shell_line.iter()).map(|s| s.to_string()).collect());
    }
    for args in tries {
        if Command::new(&args[0]).args(&args[1..]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().is_ok() {
            return Ok(());
        }
    }
    anyhow::bail!("no terminal emulator was found (set TERMINAL)")
}
