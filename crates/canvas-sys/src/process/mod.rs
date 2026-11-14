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

/// A process an agent's terminal started, as [`leftovers`] judges it once
/// the agent exits or is stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leftover {
    pub pid: u32,
    pub parent: u32,
    /// Its start time: it is ended only while it is still this process.
    pub started: u64,
    /// Lowercase executable name, as [`process_entries`] gives it.
    pub name: String,
    /// Still attached to the agent's terminal (Unix: in its session, as
    /// `session_of` tells; on Windows closing the pseudoconsole ends those).
    pub attached: bool,
}

/// What ends with an agent (R2-B-01), from what its terminal started
/// (`found`): the processes still attached to its terminal (they would keep
/// it open), and the agent's own program (`program`: processes running the
/// root's executable, each started by the root or by another of them, as a
/// launcher starts the CLI that holds the session's lock), whatever their
/// console. Everything else is left running as it was before the job of
/// S3-14: what got a console of its own or none (a server started with
/// `Start-Process`, a VS Code, MSBuild nodes, a compiler or cache server
/// other sessions use), what moved to a session of its own (a daemon), and
/// what broke away. Answers (pid, start time, own program).
pub fn leftovers(root: u32, root_started: u64, program: Option<&str>, found: &[Leftover]) -> Vec<(u32, u64, bool)> {
    let mut own: std::collections::HashMap<u32, u64> = std::collections::HashMap::from([(root, root_started)]);
    if let Some(program) = program {
        loop {
            let before = own.len();
            for process in found {
                if process.name == program && !own.contains_key(&process.pid) && own.get(&process.parent).is_some_and(|parent| process.started >= *parent) {
                    own.insert(process.pid, process.started);
                }
            }
            if own.len() == before {
                break;
            }
        }
    }
    found
        .iter()
        .filter(|p| p.pid != 0 && p.pid != root && p.pid != std::process::id())
        .filter_map(|p| {
            let mine = own.get(&p.pid) == Some(&p.started);
            (p.attached || mine).then_some((p.pid, p.started, mine))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (pid, parent, started, name, attached)
    fn found(rows: &[(u32, u32, u64, &str, bool)]) -> Vec<Leftover> {
        rows.iter().map(|&(pid, parent, started, name, attached)| Leftover { pid, parent, started, name: name.into(), attached }).collect()
    }

    /// R2-B-01: an agent (root 10, `copilot.exe`, gone) leaves what it
    /// started; only what still holds its terminal or its session ends.
    #[test]
    fn an_agent_takes_only_its_own_program_and_what_is_still_attached_to_its_terminal() {
        let found = found(&[
            // The CLI the launcher started, in a console of its own: the agent itself (holds the lock).
            (11, 10, 1_001, "copilot.exe", false),
            // A worker of that CLI.
            (12, 11, 1_002, "copilot.exe", false),
            // An MCP server on the terminal's console.
            (13, 11, 1_003, "node.exe", true),
            // A shell the agent ran, then a server it started with Start-Process (a console of its own).
            (14, 11, 1_004, "pwsh.exe", false),
            (15, 14, 1_005, "node.exe", false),
            // A nested `copilot -p` run by that shell: not the agent's own program (a shell is between).
            (16, 14, 1_006, "copilot.exe", false),
            // A compiler server and a VS Code without a console: shared or detached.
            (17, 14, 1_007, "vbcscompiler.exe", false),
            (18, 1, 1_008, "code.exe", false),
            // A pid of the root's program started before the root: never its child.
            (19, 10, 999, "copilot.exe", false),
        ]);
        let mut ended = leftovers(10, 1_000, Some("copilot.exe"), &found);
        ended.sort();
        assert_eq!(ended, [(11, 1_001, true), (12, 1_002, true), (13, 1_003, false)]);
        // Without the program rule (Unix: the session says it all), only what is attached.
        assert_eq!(leftovers(10, 1_000, None, &found), [(13, 1_003, false)]);
        // Nothing attached, nothing of the program: nothing ends.
        assert!(leftovers(10, 1_000, Some("opencode.exe"), &found[3..]).is_empty());
    }
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
        let me = std::process::id();
        assert_eq!(suspend_tree(me, process_start_ms(me).unwrap()).unwrap(), 0, "never this process, nor its children");
        let before = count.load(SeqCst);
        wait_for("the ticker stopped without being frozen", || count.load(SeqCst) > before + 2);

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
