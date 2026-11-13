//! The priority manager's rules (README "The session you type in comes
//! first"); the service's `priority_loop` applies them every 2 s.
//!
//! Only the agent itself and its console host are raised for the session
//! you type in: the tools, builds, test runners and MCP servers it starts
//! never run above normal, so the terminal pipeline (the service, above
//! normal, and the console host that carries every keystroke and echo)
//! always outranks the work the agent does. A session counts as the one you
//! type in for [`FOCUS_LAPSE`] after its last keystroke, so nothing keeps a
//! raised class forever.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use canvas_sys::process::Class;

/// The session you typed in last stays first this long without a keystroke.
pub const FOCUS_LAPSE: Duration = Duration::from_secs(60);

/// The session that last received keystrokes, and when.
#[derive(Clone, Debug, Default)]
pub struct Focus {
    last: Option<(String, Instant)>,
}

impl Focus {
    /// Terminal `id` got a keystroke at `now`. `true` when that changes which
    /// session comes first (another one, or this one again after a lapse).
    pub fn note(&mut self, id: &str, now: Instant) -> bool {
        let changed = self.current(now) != Some(id);
        self.last = Some((id.to_owned(), now));
        changed
    }

    /// The session that comes first at `now`, if any.
    pub fn current(&self, now: Instant) -> Option<&str> {
        self.last.as_ref().filter(|(_, at)| now.saturating_duration_since(*at) < FOCUS_LAPSE).map(|(id, _)| id.as_str())
    }
}

/// The process table: pid → (parent pid, lowercase executable name).
pub type Procs = HashMap<u32, (u32, String)>;

/// One live terminal as the priority manager sees it.
#[derive(Clone, Debug)]
pub struct Session {
    /// The terminal's first process (the agent, or the program it runs).
    pub root: u32,
    /// Its pseudoconsole host (Windows), when known and still the same process.
    pub console_host: Option<u32>,
    pub class: Class,
}

fn rank(class: Class) -> u8 {
    match class {
        Class::Background => 0,
        Class::Visible => 1,
        Class::Focused => 2,
    }
}

fn higher(a: Class, b: Class) -> Class {
    if rank(a) >= rank(b) { a } else { b }
}

fn lower(a: Class, b: Class) -> Class {
    if rank(a) <= rank(b) { a } else { b }
}

/// The class of every process of `sessions`. `procs` is the process table
/// (pid → parent, lowercase name); `tree` gives a root and its descendants,
/// root first.
/// - The agent: the root, and its children running the same program (a
///   launcher and the CLI it starts, as Copilot's `copilot.exe` does): the
///   session's class.
/// - Its console host: the session's class but never below normal, so the
///   process carrying keystrokes and echo never runs below what it serves.
/// - Everything else it starts: the session's class but never above normal.
pub fn classify(sessions: &[Session], procs: &Procs, tree: impl Fn(u32) -> Vec<u32>) -> HashMap<u32, Class> {
    let mut classes: HashMap<u32, Class> = HashMap::new();
    let mut set = |pid: u32, class: Class| {
        classes.entry(pid).and_modify(|c| *c = higher(*c, class)).or_insert(class);
    };
    for session in sessions {
        let program = procs.get(&session.root).map(|(_, name)| name.as_str());
        for pid in tree(session.root) {
            let agent = pid == session.root || procs.get(&pid).is_some_and(|(parent, name)| *parent == session.root && Some(name.as_str()) == program);
            set(pid, if agent { session.class } else { lower(session.class, Class::Visible) });
        }
        if let Some(host) = session.console_host {
            set(host, higher(session.class, Class::Visible));
        }
    }
    classes
}

/// Applies `classes` through `set` (which says whether it worked), skipping
/// processes that already have theirs; forgets processes no longer listed.
/// Raised processes are lowered again as soon as their class changes.
pub fn reapply(applied: &mut HashMap<u32, Class>, classes: &HashMap<u32, Class>, mut set: impl FnMut(u32, Class) -> bool) {
    for (&pid, &class) in classes {
        if applied.get(&pid) != Some(&class) && set(pid, class) {
            applied.insert(pid, class);
        }
    }
    applied.retain(|pid, _| classes.contains_key(pid));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process table: (pid, parent, name).
    fn table(rows: &[(u32, u32, &str)]) -> Procs {
        rows.iter().map(|(pid, parent, name)| (*pid, (*parent, (*name).to_owned()))).collect()
    }

    fn tree_of(procs: &Procs) -> impl Fn(u32) -> Vec<u32> + '_ {
        move |root| {
            let mut found = vec![root];
            let mut index = 0;
            while index < found.len() {
                let parent = found[index];
                let mut children: Vec<u32> = procs.iter().filter(|(pid, (p, _))| *p == parent && **pid != parent).map(|(pid, _)| *pid).collect();
                children.sort();
                found.extend(children);
                index += 1;
            }
            found
        }
    }

    /// Two terminals, each with an agent (a launcher and its CLI), a build
    /// below it, and a console host started by the service (pid 1).
    fn machine() -> Procs {
        table(&[
            (1, 0, "atelier.exe"),
            (10, 1, "conhost.exe"),
            (11, 1, "copilot.exe"),
            (12, 11, "copilot.exe"),
            (13, 12, "copilot.exe"),
            (14, 12, "pwsh.exe"),
            (15, 14, "cargo.exe"),
            (16, 15, "rustc.exe"),
            (20, 1, "conhost.exe"),
            (21, 1, "copilot.exe"),
            (22, 21, "node.exe"),
        ])
    }

    #[test]
    fn only_the_agent_and_its_console_host_are_raised_for_the_session_you_type_in() {
        let procs = machine();
        let sessions = [
            Session { root: 11, console_host: Some(10), class: Class::Focused },
            Session { root: 21, console_host: Some(20), class: Class::Background },
        ];
        let classes = classify(&sessions, &procs, tree_of(&procs));
        assert_eq!(classes[&11], Class::Focused, "the agent");
        assert_eq!(classes[&12], Class::Focused, "the CLI its launcher started");
        assert_eq!(classes[&10], Class::Focused, "its console host, which carries the keystrokes and the echo");
        for tool in [13, 14, 15, 16] {
            assert_eq!(classes[&tool], Class::Visible, "pid {tool}: workers, shells and builds are never raised");
        }
        assert_eq!(classes[&21], Class::Background);
        assert_eq!(classes[&22], Class::Background, "a background session's tools yield too");
        assert_eq!(classes[&20], Class::Visible, "a console host never runs below normal");
        assert!(!classes.contains_key(&1), "never the service itself");
    }

    #[test]
    fn a_visible_session_and_one_without_a_known_console_host() {
        let procs = machine();
        let sessions = [Session { root: 11, console_host: None, class: Class::Visible }];
        let classes = classify(&sessions, &procs, tree_of(&procs));
        assert!([11, 12, 13, 14, 15, 16].iter().all(|pid| classes[pid] == Class::Visible));
        assert!(!classes.contains_key(&10), "an unknown console host is left alone");
    }

    #[test]
    fn moving_the_focus_lowers_what_was_raised() {
        let procs = machine();
        let mut applied = HashMap::new();
        let mut calls: Vec<(u32, Class)> = Vec::new();
        let first = classify(
            &[Session { root: 11, console_host: Some(10), class: Class::Focused }, Session { root: 21, console_host: Some(20), class: Class::Visible }],
            &procs,
            tree_of(&procs),
        );
        reapply(&mut applied, &first, |pid, class| {
            calls.push((pid, class));
            true
        });
        assert_eq!(calls.len(), first.len(), "every process once");
        calls.clear();
        reapply(&mut applied, &first, |pid, class| {
            calls.push((pid, class));
            true
        });
        assert!(calls.is_empty(), "nothing again while nothing changed");
        // You type in the other session: the first one's agent and host go back to normal.
        let second = classify(
            &[Session { root: 11, console_host: Some(10), class: Class::Visible }, Session { root: 21, console_host: Some(20), class: Class::Focused }],
            &procs,
            tree_of(&procs),
        );
        reapply(&mut applied, &second, |pid, class| {
            calls.push((pid, class));
            true
        });
        calls.sort_by_key(|(pid, _)| *pid);
        assert_eq!(calls, [(10, Class::Visible), (11, Class::Visible), (12, Class::Visible), (20, Class::Focused), (21, Class::Focused)]);
        // A process that could not be set is tried again next time.
        let mut failing = HashMap::new();
        reapply(&mut failing, &second, |pid, _| pid != 21);
        assert!(!failing.contains_key(&21));
        // Processes that ended are forgotten.
        reapply(&mut applied, &HashMap::new(), |_, _| true);
        assert!(applied.is_empty());
    }

    #[test]
    fn the_session_you_type_in_lapses_without_keystrokes() {
        let start = Instant::now();
        let mut focus = Focus::default();
        assert_eq!(focus.current(start), None);
        assert!(focus.note("a", start), "a session comes first");
        assert!(!focus.note("a", start + Duration::from_secs(1)), "more keys in it change nothing");
        assert_eq!(focus.current(start + Duration::from_secs(30)), Some("a"));
        assert_eq!(focus.current(start + Duration::from_secs(1) + FOCUS_LAPSE), None, "a minute without keys: no longer first");
        assert!(focus.note("a", start + Duration::from_secs(2) + FOCUS_LAPSE), "typing again raises it again");
        assert!(focus.note("b", start + Duration::from_secs(3) + FOCUS_LAPSE));
    }

    /// A real terminal (cmd.exe running ping): its console host is the
    /// service's child, follows the agent's class and never goes below
    /// normal; the tool it runs is never raised. Only processes this test
    /// starts are changed.
    #[test]
    #[cfg(windows)]
    fn a_terminals_console_host_follows_its_agent_and_its_tools_are_never_raised() {
        use crate::emulator::Engine;
        use crate::term::{Hooks, Spawn, Terminal};
        use canvas_sys::process::{is_console_host, priority, process_entries, process_start_ms, process_tree, set_priority};
        let (pump, _pump) = crossbeam_channel::bounded(1);
        let (exited, _exited) = crossbeam_channel::bounded(16);
        let (clipboard, _clipboard) = crossbeam_channel::bounded(16);
        let (changed, _changed) = crossbeam_channel::bounded(1024);
        let hooks = Hooks { pump, exited, clipboard, bytes: Default::default(), changed, notify: None, raw: None };
        let cwd = std::env::temp_dir().display().to_string();
        let args = ["/d".to_owned(), "/c".to_owned(), "ping -n 30 127.0.0.1 >nul".to_owned()];
        let spec = Spawn { id: "priorities", program: "cmd.exe", args: &args, env: &[], cwd: &cwd, cols: 80, rows: 24, copilot: false, engine: Engine::Alacritty };
        let terminal = Terminal::spawn(spec, hooks).expect("spawn cmd.exe");
        let (console_host, started) = terminal.console_host.expect("the terminal's console host is known");
        let table = || -> (Procs, HashMap<u32, Vec<u32>>) {
            let mut procs = HashMap::new();
            let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
            for (pid, parent, name) in process_entries() {
                children.entry(parent).or_default().push(pid);
                procs.insert(pid, (parent, name));
            }
            (procs, children)
        };
        let (procs, _) = table();
        assert!(
            procs.get(&console_host).is_some_and(|(parent, name)| *parent == std::process::id() && is_console_host(name)),
            "a console host started by this process: {:?}",
            procs.get(&console_host)
        );
        assert_eq!(process_start_ms(console_host), Some(started));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let tool = loop {
            let (procs, _) = table();
            if let Some((pid, _)) = procs.iter().find(|(_, (parent, name))| *parent == terminal.pid && name == "ping.exe") {
                break *pid;
            }
            assert!(std::time::Instant::now() < deadline, "ping started");
            std::thread::sleep(Duration::from_millis(50));
        };
        let mut applied = HashMap::new();
        for (class, host_class, tool_class) in
            [(Class::Focused, Class::Focused, Class::Visible), (Class::Background, Class::Visible, Class::Background), (Class::Visible, Class::Visible, Class::Visible)]
        {
            let (procs, children) = table();
            let sessions = [Session { root: terminal.pid, console_host: Some(console_host), class }];
            reapply(&mut applied, &classify(&sessions, &procs, |root| process_tree(root, &children)), set_priority);
            assert_eq!(priority(terminal.pid), Some(class), "the agent");
            assert_eq!(priority(console_host), Some(host_class), "its console host");
            assert_eq!(priority(tool), Some(tool_class), "the tool it runs");
        }
        terminal.force_kill();
    }
}
