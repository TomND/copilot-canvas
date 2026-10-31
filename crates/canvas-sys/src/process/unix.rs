//! macOS and Linux: POSIX signals and process groups over the process table
//! of `os` (libproc on macOS, `/proc` on Linux).
//!
//! Terminals run in their own sessions (`portable-pty` calls `setsid`), so a
//! terminal's process group id is its first process's pid.

use std::io;

use super::Class;
use super::os;

pub use os::{process_argv, process_entries, process_start_ms};

#[derive(Debug, Default, Clone, Copy)]
pub struct Usage {
    /// Resident memory.
    pub working_set: u64,
    /// Memory owned by the process (macOS: the physical footprint; Linux:
    /// resident minus shared).
    pub private: u64,
    /// User plus system CPU time.
    pub cpu_ms: u64,
}

/// Memory and CPU time of `pid` (`None`: this process).
pub fn process_usage(pid: Option<u32>) -> Usage {
    os::usage(pid.unwrap_or_else(std::process::id))
}

/// True while `pid` runs (zombies do not count).
pub fn alive(pid: u32) -> bool {
    process_start_ms(pid).is_some()
}

/// Pids that may be signalled: never 0 (our own group), 1 (launchd/init) or
/// this process.
fn signallable(pid: u32) -> bool {
    pid > 1 && pid <= i32::MAX as u32 && pid != std::process::id()
}

fn signal(pid: u32, signal: i32) -> bool {
    // SAFETY: plain kill(2) on a checked pid.
    signallable(pid) && unsafe { libc::kill(pid as i32, signal) } == 0
}

/// `root` and its descendants by parent links (children started after their
/// parent), breadth first.
fn tree(root: u32) -> Vec<u32> {
    let mut children: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
    for (pid, parent, _) in process_entries() {
        children.entry(parent).or_default().push(pid);
    }
    process_tree(root, &children)
}

/// Kills `root` and every descendant process (deepest first), then whatever
/// is left of `root`'s process group when `root` leads one (its terminal's
/// session: processes that re-parented away stay in it).
pub fn kill_tree(root: u32) -> io::Result<usize> {
    if !signallable(root) {
        return Ok(0);
    }
    // SAFETY: getpgid on a checked pid.
    let leads_group = unsafe { libc::getpgid(root as i32) } == root as i32;
    let mut killed = 0;
    for pid in tree(root).into_iter().rev() {
        if signal(pid, libc::SIGKILL) {
            killed += 1;
        }
    }
    if leads_group && root as i32 != unsafe { libc::getpgrp() } {
        // SAFETY: a process group other than ours.
        unsafe { libc::killpg(root as i32, libc::SIGKILL) };
    }
    Ok(killed)
}

/// Descendant process ids of `root` (excluding root).
pub fn descendants(root: u32) -> Vec<u32> {
    let pairs = process_pairs();
    let mut found = vec![root];
    let mut index = 0;
    while index < found.len() {
        let parent = found[index];
        for &(pid, ppid) in &pairs {
            if ppid == parent && pid != parent && !found.contains(&pid) {
                found.push(pid);
            }
        }
        index += 1;
    }
    found.remove(0);
    found
}

/// Keeps descendants from outliving this process where the OS allows it:
/// Linux makes this process their subreaper (orphans re-parent here instead
/// of to init, so the service still sees and stops them); macOS has no
/// subreaper, so the service remembers terminals' trees instead
/// (MAC-LINUX-PLAN §5.2). Terminals also get `SIGHUP` from their PTY when
/// the service dies.
pub fn contain_descendants() -> io::Result<()> {
    os::contain_descendants()
}

/// A managed helper process (`devtunnel connect`) that is killed with its
/// process tree when this guard is dropped. Unlike a Windows job, an abrupt
/// exit of this process does not kill it; helpers that must never outlive us
/// are also spawned with [`crate::spawn::CommandExt::die_with_parent`].
pub struct KillOnClose {
    pid: u32,
    started: Option<u64>,
}

impl KillOnClose {
    pub fn new(child: &std::process::Child) -> io::Result<Self> {
        let pid = child.id();
        Ok(Self { pid, started: process_start_ms(pid) })
    }
}

impl Drop for KillOnClose {
    fn drop(&mut self) {
        if self.started.is_some() && process_start_ms(self.pid) == self.started {
            let _ = kill_tree(self.pid);
        }
    }
}

/// Gives this process's calling thread the scheduler's interactive class
/// where the OS has one (macOS QoS user-interactive); Unix does not let an
/// unprivileged process raise its priority, so the priority manager lowers
/// the others instead.
pub fn raise_priority() {
    os::raise_self();
}

/// Applies a session class to `pid`: macOS puts background sessions in the
/// `PRIO_DARWIN_BG` band (lower CPU and disk priority, efficiency cores) and
/// clears it for visible and focused ones; Linux renices background
/// sessions to 5 (unprivileged processes cannot undo it, so it is one-way).
pub fn set_priority(pid: u32, class: Class) -> bool {
    signallable(pid) && os::set_priority(pid, class)
}

/// The user a process runs as (`uid:N`), to compare with [`current_user_id`].
pub fn process_user_id(pid: u32) -> io::Result<String> {
    os::process_uid(pid).map(|uid| format!("uid:{uid}"))
}

/// Open file descriptors of `pid` (the soak test watches for leaks).
pub fn handle_count(pid: u32) -> u32 {
    os::fd_count(pid)
}

/// This process's user (`uid:N`, the effective uid).
pub fn current_user_id() -> io::Result<String> {
    // SAFETY: geteuid cannot fail.
    Ok(format!("uid:{}", unsafe { libc::geteuid() }))
}

/// `root` and all its descendants, in O(processes), given each pid's
/// children. Children must have started after their parent.
pub fn process_tree(root: u32, children: &std::collections::HashMap<u32, Vec<u32>>) -> Vec<u32> {
    let mut found = vec![root];
    let mut index = 0;
    while index < found.len() {
        let parent = found[index];
        for &child in children.get(&parent).map(Vec::as_slice).unwrap_or_default() {
            if child != parent && !found.contains(&child) && started_after(child, parent) {
                found.push(child);
            }
        }
        index += 1;
    }
    found
}

/// Whether `child` started no earlier than `parent` (unknown parent: yes).
pub fn started_after(child: u32, parent: u32) -> bool {
    match (process_start_ms(child), process_start_ms(parent)) {
        (Some(c), Some(p)) => c >= p,
        (Some(_), None) => true,
        _ => false,
    }
}

/// (pid, parent pid) of every process.
pub fn process_pairs() -> Vec<(u32, u32)> {
    process_entries().into_iter().map(|(pid, parent, _)| (pid, parent)).collect()
}

/// True when `pid` is running and was already running when `lock` was last written.
/// Guards against a recycled PID making a stale lock look live.
pub fn lock_owner_alive(pid: u32, lock: &std::path::Path) -> bool {
    let Some(started) = process_start_ms(pid) else { return false };
    let written = std::fs::metadata(lock)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64);
    match written {
        Some(written) => started <= written + 2_000,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_process_table_knows_this_process_and_its_command_line() {
        let me = std::process::id();
        assert!(alive(me));
        let started = process_start_ms(me).unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        assert!(started <= now && now - started < 600_000, "started {started}, now {now}");
        let parent = std::os::unix::process::parent_id();
        assert!(process_entries().iter().any(|(pid, ppid, _)| *pid == me && *ppid == parent));
        let argv = process_argv(me).unwrap();
        assert_eq!(argv.len(), std::env::args().count(), "{argv:?}");
        let usage = process_usage(None);
        assert!(usage.working_set > 0);
        assert_eq!(process_user_id(me).unwrap(), current_user_id().unwrap());
        assert!(!alive(0));
    }

    #[test]
    fn kill_tree_ends_a_process_group_and_its_grandchildren() {
        use std::os::unix::process::CommandExt as _;
        // A shell (its own group) with a background grandchild.
        let mut child = std::process::Command::new("/bin/sh").args(["-c", "sleep 30 & sleep 30; wait"]).process_group(0).spawn().unwrap();
        let root = child.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while descendants(root).len() < 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let below = descendants(root);
        assert_eq!(below.len(), 2, "{below:?}");
        assert!(kill_tree(root).unwrap() >= 1);
        child.wait().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while below.iter().any(|&pid| alive(pid)) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(below.iter().all(|&pid| !alive(pid)), "grandchildren are gone");
        assert_eq!(kill_tree(0).unwrap(), 0, "pid 0 (our own group) is never signalled");
    }

    #[test]
    fn background_sessions_get_the_lower_class() {
        let mut child = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
        assert!(set_priority(child.id(), Class::Background));
        assert!(set_priority(child.id(), Class::Visible));
        assert!(!set_priority(0, Class::Background));
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
