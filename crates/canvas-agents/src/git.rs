//! Git discovery and worktree creation. All commands run off the UI thread.

use canvas_sys::spawn::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};


pub use canvas_model::git::{GitInfo, default_worktree_path, slug, valid_branch_name};

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .no_window()
        .output()
        .context("git is not installed or not on PATH")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Returns `Ok(None)` when `dir` is not inside a Git work tree.
pub fn probe(dir: &Path) -> Result<Option<GitInfo>> {
    anyhow::ensure!(dir.is_dir(), "not a folder: {}", dir.display());
    let root = match git(dir, &["rev-parse", "--show-toplevel"]) {
        Ok(root) => PathBuf::from(root.trim()),
        Err(error) => {
            let text = error.to_string();
            if text.contains("not a git repository") {
                return Ok(None);
            }
            return Err(error);
        }
    };
    let branch = git(dir, &["branch", "--show-current"]).ok().map(|b| b.trim().to_owned()).filter(|b| !b.is_empty());
    let local = git(dir, &["for-each-ref", "--sort=-committerdate", "--count=300", "--format=%(refname:short)", "refs/heads"])?;
    let remote = git(dir, &["for-each-ref", "--sort=-committerdate", "--count=300", "--format=%(refname:short)", "refs/remotes"])?;
    let mut branches: Vec<String> = local.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect();
    let local_count = branches.len();
    branches.extend(remote.lines().map(str::trim).filter(|l| !l.is_empty() && !l.ends_with("/HEAD") && l.contains('/')).map(str::to_owned));
    Ok(Some(GitInfo { root: PathBuf::from(root.to_string_lossy().replace('/', "\\")), branch, branches, local_count }))
}

/// `git worktree add`, creating `new_branch` from `base`, or checking out `base` if no new branch.
pub fn add_worktree(root: &Path, path: &Path, new_branch: Option<&str>, base: &str) -> Result<()> {
    anyhow::ensure!(!path.exists() || path.read_dir().map(|mut d| d.next().is_none()).unwrap_or(false), "{} already exists and is not empty", path.display());
    let path_text = path.to_string_lossy().into_owned();
    let mut args = vec!["worktree", "add"];
    if let Some(branch) = new_branch {
        anyhow::ensure!(valid_branch_name(branch), "invalid branch name: {branch}");
        args.extend(["-b", branch, path_text.as_str(), base]);
    } else {
        anyhow::ensure!(!base.is_empty(), "choose a branch to check out");
        args.extend([path_text.as_str(), base]);
    }
    git(root, &args).map(|_| ()).context("git worktree add failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_repositories_and_creates_worktrees() {
        let dir = std::env::temp_dir().join(format!("cc-git-{}", uuid::Uuid::new_v4()));
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        assert_eq!(probe(&dir).unwrap(), None, "plain folders are not repositories");
        git(&repo, &["init", "-q", "-b", "main"]).unwrap();
        git(&repo, &["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-q", "--allow-empty", "-m", "init"]).unwrap();
        let info = probe(&repo).unwrap().unwrap();
        assert_eq!(info.branch.as_deref(), Some("main"));
        assert_eq!(info.branches, vec!["main".to_owned()]);
        let worktree = default_worktree_path(&info.root, "feature/one");
        add_worktree(&info.root, &worktree, Some("feature/one"), "main").unwrap();
        let wt = probe(&worktree).unwrap().unwrap();
        assert_eq!(wt.branch.as_deref(), Some("feature/one"));
        assert!(add_worktree(&info.root, &worktree, Some("feature/two"), "main").is_err(), "non-empty target refused");
        git(&repo, &["worktree", "remove", "--force", &worktree.to_string_lossy()]).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
