//! Opening things for the user: URLs in the browser, folders in the file
//! manager, files revealed in it or opened in VS Code. Nothing is ever
//! executed by opening: on Unix files are never handed to `open` /
//! `xdg-open` when they could run (an executable, an `.app`, a `.desktop`
//! file); those are revealed instead (MAC-LINUX-PLAN §5.1).

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

/// `vscode://file/<path>[:line]` for a local file.
pub fn vscode_file_url(path: &std::path::Path, line: Option<u32>) -> String {
    let mut url = format!("vscode://file/{}", path.display().to_string().replace('\\', "/").trim_start_matches('/'));
    if let Some(line) = line {
        url.push_str(&format!(":{line}"));
    }
    url
}
