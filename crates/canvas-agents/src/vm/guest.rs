//! Guest-side commands of `atelier satellite` that the hub runs through a
//! provider's `exec` (no shell, so the same on every platform): writing and
//! reading files the hub sends (`--write-file`, `--read-file`), cloning a
//! repository into an image (`--clone`) and preparing a claimed sandbox's
//! branch (`--prepare`). Paths are relative to the guest's home unless they
//! are absolute; repositories live in `<guest home>/work/<repo key>`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// The guest's home: `HOME` (also on Windows, where local test sandboxes set
/// it and the user's folder would otherwise be the real one), else
/// `USERPROFILE`, else the user's folder.
pub fn guest_home() -> PathBuf {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).filter(|h| !h.is_empty()).map(PathBuf::from).unwrap_or_else(crate::agents::home)
}

/// `path` in the guest: absolute as given, else under the guest's home
/// (never with `..`).
pub fn guest_path(path: &str) -> Result<PathBuf> {
    anyhow::ensure!(!path.is_empty() && !path.split(['/', '\\']).any(|p| p == ".."), "not a usable path: {path:?}");
    let p = Path::new(path);
    Ok(if p.is_absolute() { p.to_path_buf() } else { guest_home().join(path) })
}

/// Where repository `key` is cloned in the guest.
pub fn workspace(key: &str) -> Result<PathBuf> {
    anyhow::ensure!(!key.is_empty() && key.len() <= 60 && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'), "not a repository key: {key:?}");
    Ok(guest_home().join("work").join(key))
}

/// `--write-file PATH [--mode OCTAL]`: standard input into PATH (parents
/// made, written atomically, with `mode` on Unix: 0600 by default).
pub fn write_file(path: &str, mode: Option<&str>) -> Result<Value> {
    let mode = match mode {
        Some(text) => u32::from_str_radix(text, 8).ok().filter(|m| *m <= 0o7777).with_context(|| format!("--mode takes an octal mode, not {text:?}"))?,
        None => 0o600,
    };
    let path = guest_path(path)?;
    let mut bytes = Vec::new();
    std::io::stdin().take(64 << 20).read_to_end(&mut bytes)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_with_mode(&path, &bytes, mode).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(json!({ "ok": true, "bytes": bytes.len() }))
}

/// Writes `path` atomically, owner-only from the start (0600), then with
/// `mode` on Unix (Windows: owner-only, `mode` is not used).
pub fn write_with_mode(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    canvas_sys::fs::write_private(path, bytes)?;
    #[cfg(unix)]
    if mode != 0o600 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(())
}

/// `--read-file PATH`: the file on standard output (raw).
pub fn read_file(path: &str) -> Result<Vec<u8>> {
    let path = guest_path(path)?;
    std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))
}

fn git(args: &[&str], cwd: &Path) -> Result<(bool, String)> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(cwd).env("GIT_TERMINAL_PROMPT", "0");
    canvas_sys::spawn::CommandExt::no_window(&mut cmd);
    let out = cmd.output().context("git is not installed in the guest")?;
    Ok((out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))))
}

/// `--clone --repo KEY --url URL [--branch B] [--mode full|blobless|single_branch]`:
/// the repository at its default branch in the guest's workspace (an image).
pub fn clone(key: &str, url: &str, branch: &str, mode: &str) -> Result<Value> {
    anyhow::ensure!(canvas_model::vm::branch_ok(branch), "not a branch name: {branch:?}");
    anyhow::ensure!(!url.starts_with('-'), "not a repository URL");
    let dir = workspace(key)?;
    if dir.join(".git").exists() {
        let (ok, out) = git(&["fetch", "--quiet", "origin", branch], &dir)?;
        anyhow::ensure!(ok, "git fetch failed: {}", super::hooks::first_line(&out));
        return Ok(json!({ "ok": true, "path": dir.display().to_string(), "fetched": true }));
    }
    std::fs::create_dir_all(dir.parent().context("workspace")?)?;
    let target = dir.display().to_string();
    let mut args = vec!["clone", "--quiet", "--branch", branch];
    match mode {
        "blobless" => args.push("--filter=blob:none"),
        "single_branch" => args.push("--single-branch"),
        _ => {}
    }
    args.extend(["--", url, &target]);
    let (ok, out) = git(&args, &guest_home())?;
    anyhow::ensure!(ok, "git clone failed: {}", super::hooks::first_line(&out));
    Ok(json!({ "ok": true, "path": target, "fetched": false }))
}

/// `--prepare --repo KEY --branch B --base MAIN`: a claimed sandbox's branch:
/// the default branch fetched, then `B` tracking `origin/B` when it exists
/// on the remote, else made from `origin/MAIN`. A branch already checked
/// out is left as it is (a running branch is never rebased).
pub fn prepare(key: &str, branch: &str, base: &str) -> Result<Value> {
    anyhow::ensure!(canvas_model::vm::branch_ok(branch) && canvas_model::vm::branch_ok(base), "not a branch name");
    let dir = workspace(key)?;
    anyhow::ensure!(dir.join(".git").exists(), "the repository {key} is not in the guest's workspace (build the image again)");
    let (_, current) = git(&["rev-parse", "--abbrev-ref", "HEAD"], &dir)?;
    if current.trim() == branch {
        return Ok(json!({ "ok": true, "path": dir.display().to_string(), "branch": branch, "state": "already" }));
    }
    let (fetched, out) = git(&["fetch", "--quiet", "origin", base], &dir)?;
    anyhow::ensure!(fetched, "git fetch failed: {}", super::hooks::first_line(&out));
    let remote = format!("origin/{branch}");
    let (exists, _) = git(&["ls-remote", "--exit-code", "--heads", "origin", branch], &dir)?;
    let (ok, out, state) = if exists {
        let _ = git(&["fetch", "--quiet", "origin", branch], &dir)?;
        let (ok, out) = git(&["checkout", "--quiet", "-B", branch, "--track", &remote], &dir)?;
        (ok, out, "tracked")
    } else {
        let (ok, out) = git(&["checkout", "--quiet", "-b", branch, &format!("origin/{base}")], &dir)?;
        (ok, out, "new")
    };
    if !ok {
        bail!("cannot check out {branch}: {}", super::hooks::first_line(&out));
    }
    Ok(json!({ "ok": true, "path": dir.display().to_string(), "branch": branch, "state": state }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_paths_stay_plain() {
        assert!(guest_path("../x").is_err() && guest_path("a/../b").is_err() && guest_path("").is_err());
        assert!(guest_path(".atelier/satellite.json").unwrap().ends_with(".atelier/satellite.json"));
        assert!(workspace("../x").is_err() && workspace("a/b").is_err() && workspace("contoso-app").is_ok());
        assert!(clone("app", "--upload-pack=x", "main", "full").is_err(), "a URL never reads as an option");
    }
}
