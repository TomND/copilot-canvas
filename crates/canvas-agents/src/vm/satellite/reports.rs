//! What the satellite reports about its guest (plan VM-23 `proc` and
//! `stats`, VM-28): the agent sessions started inside each terminal (an
//! agent running `copilot -p …`: the hub shows them as subsessions), the
//! Copilot sessions alive in the guest, and every 30 s the guest's load and
//! each terminal's repository (git ahead, behind, dirty, untracked, stashes,
//! unpushed branches).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

use canvas_common::sat::{NestedSession, ProcReport, RepoStats, StatsReport, TermProcs};

use super::terms::Running;

/// The sessions started inside the running terminals and the live sessions
/// of the guest's Copilot history folder `root`.
pub fn proc_report(running: &[Running], root: &Path) -> ProcReport {
    let entries = canvas_sys::process::process_entries();
    let mut procs: HashMap<u32, (u32, String)> = entries.iter().map(|(pid, parent, name)| (*pid, (*parent, name.clone()))).collect();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, parent, _) in &entries {
        children.entry(*parent).or_default().push(*pid);
    }
    let mut report = ProcReport::default();
    for run in running {
        let tree = canvas_sys::process::process_tree(run.pid, &children);
        crate::service::name_node_copilots(&tree, &mut procs);
        let mut nested = Vec::new();
        for pid in crate::service::nested_copilots(&tree, &procs) {
            let Some(session) = crate::service::session_for_pid(root, pid) else { continue };
            // The terminal's own session (the box's agent) is not a subsession of itself.
            if session == run.term || nested.iter().any(|n: &NestedSession| n.session == session) {
                continue;
            }
            let yaml = std::fs::read_to_string(root.join(&session).join("workspace.yaml")).unwrap_or_default();
            let field = |key: &str| yaml.lines().find_map(|l| l.strip_prefix(key)).map(|v| v.trim().trim_matches(['"', '\'']).to_owned()).unwrap_or_default();
            nested.push(NestedSession { session, name: field("name:"), cwd: field("cwd:") });
        }
        report.terms.push(TermProcs { term: run.term.clone(), generation: run.generation, nested });
    }
    let mut live: Vec<String> = std::fs::read_dir(root)
        .map(|e| e.flatten().filter_map(|e| e.file_name().to_str().map(str::to_owned)).filter(|id| crate::index::is_session_id(id)).filter(|id| crate::service::session_live(root, id)).collect())
        .unwrap_or_default();
    live.sort();
    report.live = live;
    report
}

/// The guest's load and each running terminal's repository.
pub fn stats_report(running: &[Running], home: &Path) -> StatsReport {
    let mut report = StatsReport::default();
    let entries = canvas_sys::process::process_entries();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, parent, _) in &entries {
        children.entry(*parent).or_default().push(*pid);
    }
    let mut counted = HashSet::new();
    for run in running {
        for pid in canvas_sys::process::process_tree(run.pid, &children) {
            if counted.insert(pid) {
                let usage = canvas_sys::process::process_usage(Some(pid));
                report.cpu_ms += usage.cpu_ms;
                report.memory_bytes += usage.working_set;
            }
        }
    }
    if let Ok((free, total)) = canvas_sys::fs::space(home) {
        (report.disk_free_bytes, report.disk_total_bytes) = (free, total);
    }
    if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
        (report.memory_total_bytes, report.memory_available_bytes) = meminfo(&text);
    }
    if let Ok(text) = std::fs::read_to_string("/proc/loadavg") {
        report.load_1m = text.split_whitespace().next().and_then(|l| l.parse().ok()).unwrap_or(0.0);
    }
    let mut seen = HashSet::new();
    for run in running {
        if seen.insert(run.cwd.clone())
            && let Some(mut repo) = repo_stats(Path::new(&run.cwd))
        {
            repo.term = run.term.clone();
            report.repos.push(repo);
        }
    }
    report
}

/// `MemTotal` and `MemAvailable` of `/proc/meminfo`, in bytes.
fn meminfo(text: &str) -> (u64, u64) {
    let kb = |key: &str| text.lines().find_map(|l| l.strip_prefix(key)).and_then(|v| v.split_whitespace().next()).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) * 1024;
    (kb("MemTotal:"), kb("MemAvailable:"))
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(cwd).env("GIT_TERMINAL_PROMPT", "0").env("GIT_OPTIONAL_LOCKS", "0");
    canvas_sys::spawn::CommandExt::no_window(&mut cmd);
    let out = cmd.output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The repository at `cwd` (`None`: not a repository, or no git).
pub fn repo_stats(cwd: &Path) -> Option<RepoStats> {
    let status = git(cwd, &["status", "--porcelain=v2", "--branch", "--untracked-files=normal"])?;
    let mut repo = parse_status(&status);
    repo.cwd = cwd.display().to_string();
    repo.stashes = git(cwd, &["stash", "list"]).map(|s| s.lines().count() as u32).unwrap_or(0);
    repo.unpushed = git(cwd, &["for-each-ref", "--format=%(refname:short)\t%(upstream)\t%(upstream:track)", "refs/heads"]).map(|r| parse_unpushed(&r)).unwrap_or_default();
    Some(repo)
}

/// `git status --porcelain=v2 --branch`: the branch, ahead and behind its
/// upstream, changed entries (staged or not, conflicts) and untracked files.
pub fn parse_status(text: &str) -> RepoStats {
    let mut repo = RepoStats::default();
    for line in text.lines() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            repo.branch = if head == "(detached)" { String::new() } else { head.to_owned() };
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for part in ab.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    repo.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    repo.behind = n.parse().unwrap_or(0);
                }
            }
        } else if line.starts_with("1 ") || line.starts_with("2 ") || line.starts_with("u ") {
            repo.dirty += 1;
        } else if line.starts_with("? ") {
            repo.untracked += 1;
        }
    }
    repo
}

/// `git for-each-ref --format=%(refname:short)\t%(upstream)\t%(upstream:track)`:
/// local branches with no upstream, or ahead of it.
pub fn parse_unpushed(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let (name, upstream, track) = (parts.next()?, parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            (!name.is_empty() && (upstream.is_empty() || track.contains("ahead"))).then(|| name.to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_status_and_refs_are_read() {
        let status = "# branch.oid 0123\n# branch.head atelier/x\n# branch.upstream origin/atelier/x\n# branch.ab +2 -1\n1 .M N... 100644 100644 100644 a b f.rs\n2 R. N... 100644 100644 100644 a b R100 g.rs\th.rs\nu UU N... 1 2 3 4 a b c c.rs\n? new.txt\n? other.txt\n! ignored\n";
        let repo = parse_status(status);
        assert_eq!((repo.branch.as_str(), repo.ahead, repo.behind, repo.dirty, repo.untracked), ("atelier/x", 2, 1, 3, 2));
        assert_eq!(parse_status("# branch.head (detached)\n").branch, "");
        let refs = "main\trefs/remotes/origin/main\t\natelier/x\trefs/remotes/origin/atelier/x\t[ahead 2, behind 1]\nlocal-only\t\t\ngone\trefs/remotes/origin/gone\t[gone]\n";
        assert_eq!(parse_unpushed(refs), ["atelier/x", "local-only"]);
        assert_eq!(meminfo("MemTotal:       16384 kB\nMemFree: 1 kB\nMemAvailable:    8192 kB\n"), (16384 * 1024, 8192 * 1024));
    }

    #[test]
    fn a_real_repository_is_measured() {
        let dir = std::env::temp_dir().join(format!("atelier-sat-git-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| Command::new("git").args(args).current_dir(&dir).output().is_ok_and(|o| o.status.success());
        if !run(&["init", "-q", "-b", "main"]) {
            return; // no git here
        }
        assert!(run(&["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-q", "--allow-empty", "-m", "one"]));
        std::fs::write(dir.join("untracked.txt"), b"x").unwrap();
        let repo = repo_stats(&dir).unwrap();
        assert_eq!((repo.branch.as_str(), repo.untracked, repo.dirty, repo.stashes), ("main", 1, 0, 0));
        assert_eq!(repo.unpushed, ["main"], "no upstream");
        assert!(repo_stats(&std::env::temp_dir().join("atelier-not-a-repo-at-all")).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }
}
