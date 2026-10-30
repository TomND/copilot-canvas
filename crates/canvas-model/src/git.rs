//! Git facts of a folder and the naming rules for new branches and worktrees
//! (pure; the backend runs git itself).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct GitInfo {
    pub root: PathBuf,
    pub branch: Option<String>,
    /// Local branches first (most recent commit first), then remote-tracking branches.
    pub branches: Vec<String>,
    pub local_count: usize,
}

/// Branch-name slug: lowercase, `[a-z0-9._/-]`, no leading/trailing separators.
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.trim().chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '/' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches(['-', '.', '/']).replace("..", ".").replace("//", "/");
    out.chars().take(60).collect::<String>().trim_end_matches(['-', '.', '/']).to_owned()
}

pub fn valid_branch_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['-', '/', '.'])
        && !name.ends_with(['/', '.'])
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("@{")
        && !name.contains("//")
        && name.chars().all(|c| !c.is_control() && !" ~^:?*[\\".contains(c))
}

/// Default location: `<repo-parent>\<repo-name>.worktrees\<branch-slug>`.
pub fn default_worktree_path(root: &Path, branch: &str) -> PathBuf {
    let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    let parent = root.parent().unwrap_or(root);
    parent.join(format!("{name}.worktrees")).join(slug(branch).replace('/', "-"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_and_branch_validation() {
        assert_eq!(slug("Fix the Login bug!"), "fix-the-login-bug");
        assert_eq!(slug("  feature/New Thing "), "feature/new-thing");
        assert_eq!(slug("..weird..name.."), "weird.name");
        assert!(valid_branch_name("feature/x-1"));
        assert!(!valid_branch_name("bad name"));
        assert!(!valid_branch_name("a..b"));
        assert!(!valid_branch_name("-x"));
        assert!(!valid_branch_name("x.lock"));
    }

    #[test]
    #[cfg(windows)] // Windows paths and file locks (service side)
    fn default_worktree_path_is_beside_the_repository() {
        let path = default_worktree_path(Path::new(r"C:\src\repo"), "feature/x");
        assert_eq!(path, PathBuf::from(r"C:\src\repo.worktrees\feature-x"));
    }
}
