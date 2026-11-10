//! Processes: liveness with start times (the guard against recycled pids),
//! the process table, trees, killing a tree, freezing and thawing a tree
//! (`suspend_tree`, `resume_tree`: `vm::local`'s memory suspend),
//! containment of descendants, resource usage and the priority classes of
//! the priority manager.
//!
//! Every tree walk keeps the rule that a child must have started after its
//! parent, so a recycled parent pid can never adopt unrelated processes.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

// The process table and per-process facts of each Unix.
#[cfg(macos)]
#[path = "macos.rs"]
mod os;
#[cfg(linux)]
#[path = "linux.rs"]
mod os;
#[cfg(macos)]
pub(crate) use os::all_pids;

/// How the priority manager treats a terminal's process tree
/// (README "The session you type in comes first").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Class {
    /// The session you type in.
    Focused,
    /// Sessions shown on screen.
    Visible,
    /// Every other session.
    Background,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::time::{Duration, Instant};

    /// This test binary as a helper process (the same on every OS):
    /// `ticker` prints `tick N` every 50 ms for at most a minute; `parent`
    /// runs a ticker as its child, so the two make a tree.
    fn helper(mode: &str) -> std::process::Command {
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args(["process::tests::helper_process", "--exact", "--ignored", "--nocapture", "--test-threads=1", &format!("helper:{mode}")]);
        cmd
    }

    #[test]
    #[ignore = "a helper process the suspend test runs"]
    fn helper_process() {
        let Some(mode) = std::env::args().find_map(|a| a.strip_prefix("helper:").map(str::to_owned)) else { return };
        if mode == "parent" {
            let _ = helper("ticker").status();
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut tick = 0u64;
        while Instant::now() < deadline {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "tick {tick}").and_then(|_| out.flush());
            drop(out);
            tick += 1;
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Counts the `tick` lines `child` prints.
    fn ticks(child: &mut std::process::Child) -> Arc<AtomicUsize> {
        let count = Arc::new(AtomicUsize::new(0));
        let stdout = child.stdout.take().unwrap();
        let seen = count.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                if line.starts_with("tick") {
                    seen.fetch_add(1, SeqCst);
                }
            }
        });
        count
    }

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn suspend_tree_freezes_a_tree_and_resume_tree_thaws_it() {
        let mut child = helper("parent").stdout(std::process::Stdio::piped()).spawn().unwrap();
        let root = child.id();
        let started = process_start_ms(root).unwrap();
        let count = ticks(&mut child);
        wait_for("the ticker did not start", || count.load(SeqCst) >= 3);

        assert_eq!(suspend_tree(root, started + 1).unwrap(), 0, "a pid with another start time (a reused pid) is never signalled");
        let before = count.load(SeqCst);
        wait_for("the ticker stopped without being frozen", || count.load(SeqCst) > before + 2);
        let me = std::process::id();
        assert_eq!(suspend_tree(me, process_start_ms(me).unwrap()).unwrap(), 0, "never this process");

        let frozen = suspend_tree(root, started).unwrap();
        assert!(frozen >= 2, "the parent and its ticker: {frozen}");
        // Lines already in the pipe arrive; then nothing while frozen.
        std::thread::sleep(Duration::from_millis(300));
        let frozen_at = count.load(SeqCst);
        std::thread::sleep(Duration::from_millis(700));
        assert_eq!(count.load(SeqCst), frozen_at, "no output while frozen");
        assert!(alive(root), "a frozen process is alive");

        let resumed = resume_tree(root, started).unwrap();
        assert!(resumed >= 2, "{resumed}");
        wait_for("no output after resume", || count.load(SeqCst) > frozen_at + 2);

        assert!(kill_tree(root).unwrap() >= 1);
        child.wait().unwrap();
        assert_eq!(resume_tree(root, started).unwrap(), 0, "an ended tree is not signalled");
    }
}
