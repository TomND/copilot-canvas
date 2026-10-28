//! Manual probes against the real agent stores and CLIs on this machine
//! (`cargo test -p canvas-agents -- --ignored --nocapture probe_`). They read
//! history read-only; the OpenCode end-to-end probe uses a free model only and
//! the Claude probe never sends a prompt.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use canvas_host::emulator::{Emulator, feed_collect};
use canvas_host::term::{headless, lock, screen_text};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use super::claude::{Claude, ClaudeHome};
use super::claude_history::{ClaudeHistory, claude_dir, live_sessions};
use super::opencode::OpenCode;
use super::opencode_history::{OpenCodeHistory, db_path, discover_new_session};
use super::{Command, Integration, Provider, opencode_server};
use crate::paths::now_ms;
use crate::protocol::{Activity, AgentLaunch};

#[derive(Default)]
struct Seen {
    titles: Vec<String>,
    progress: Vec<u8>,
    bytes: usize,
}

/// A program in a pseudoconsole whose output feeds a headless emulator.
struct Console {
    screen: Arc<Mutex<canvas_host::emulator::alacritty::AlacrittyEmulator>>,
    seen: Arc<Mutex<Seen>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl Console {
    fn start(command: &Command, cwd: &Path, cols: u16, rows: u16) -> anyhow::Result<Self> {
        let pair = native_pty_system().openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
        let mut builder = CommandBuilder::new(&command.program);
        builder.args(&command.args);
        builder.cwd(cwd);
        builder.env("TERM", "xterm-256color");
        builder.env("COLORTERM", "truecolor");
        for (key, value) in &command.env {
            if value.is_empty() && key.starts_with('-') {
                builder.env_remove(&key[1..]);
            } else {
                builder.env(key, value);
            }
        }
        let child = pair.slave.spawn_command(builder)?;
        drop(pair.slave);
        let screen = Arc::new(Mutex::new(headless(cols, rows, true)));
        let seen = Arc::new(Mutex::new(Seen::default()));
        let mut reader = pair.master.try_clone_reader()?;
        let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(pair.master.take_writer()?));
        let (shared, events, replies) = (screen.clone(), seen.clone(), writer.clone());
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 65536];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let collected = feed_collect(&mut *lock(&shared), &buf[..n]);
                {
                    let mut seen = lock(&events);
                    seen.bytes += n;
                    if let Some(Some(title)) = collected.title
                        && seen.titles.last() != Some(&title)
                    {
                        seen.titles.push(title);
                    }
                    seen.progress.extend(collected.progress);
                }
                for reply in collected.replies {
                    let mut w = lock(&replies);
                    let _ = w.write_all(&reply).and_then(|_| w.flush());
                }
            }
        });
        Ok(Self { screen, seen, writer, child, _master: pair.master })
    }

    fn text(&self) -> String {
        screen_text(&lock(&self.screen).snapshot())
    }

    fn send(&self, bytes: &[u8]) {
        let mut w = lock(&self.writer);
        let _ = w.write_all(bytes).and_then(|_| w.flush());
    }

    fn type_text(&self, text: &str) {
        for c in text.chars() {
            self.send(c.to_string().as_bytes());
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    fn wait(&self, timeout: Duration, mut predicate: impl FnMut(&Self) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if predicate(self) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    fn exited(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_some()
    }

    fn stop(&mut self) {
        for _ in 0..2 {
            self.send(b"\x03");
            std::thread::sleep(Duration::from_millis(700));
        }
        if !self.wait_exit(Duration::from_secs(5)) {
            if let Some(pid) = self.child.process_id() {
                let _ = crate::win::kill_tree(pid);
            }
            let _ = self.child.kill();
        }
    }

    fn wait_exit(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.exited() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cc-probe-{name}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn age(now: u64, ms: u64) -> String {
    if ms == 0 { "-".into() } else { format!("{}s", now.saturating_sub(ms) / 1000) }
}

#[test]
#[ignore]
fn probe_claude_store() {
    let dir = claude_dir();
    let mut history = ClaudeHistory::new(dir.clone());
    let started = Instant::now();
    let (all, _) = history.poll(&HashSet::new(), Duration::ZERO);
    println!("claude: {} sessions listed in {:?} from {}", all.len(), started.elapsed(), dir.display());
    let live = live_sessions(&dir);
    let mut recent = all.clone();
    recent.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
    let tracked: HashSet<String> = live.keys().cloned().chain(recent.iter().take(4).map(|s| s.id.clone())).collect();
    let started = Instant::now();
    let (changed, _) = history.poll(&tracked, Duration::from_secs(60));
    println!("tracked {} (live pid files: {}) first poll {:?}", tracked.len(), live.len(), started.elapsed());
    let now = now_ms();
    for s in changed.iter().filter(|s| s.tracked) {
        let pid = live.get(&s.id).map(|p| format!("pid {} status={} waitingFor={:?}", p.pid, p.status, p.waiting_for)).unwrap_or_else(|| "no live pid file".into());
        println!(
            "  {} {:<40} live={} {:?} pending={} finished={} last_event={} truncated={} agents={} ({} working) branch={} cwd={} [{pid}]",
            &s.id[..8],
            s.name.chars().take(40).collect::<String>(),
            s.live,
            s.activity,
            s.pending,
            s.finished_ms.map(|f| age(now, f)).unwrap_or("-".into()),
            age(now, s.last_event_ms),
            s.truncated,
            s.agents.len(),
            s.agents.iter().filter(|a| a.status == crate::protocol::AgentStatus::Working).count(),
            s.branch,
            s.cwd,
        );
    }
    let mut worst = Duration::ZERO;
    for _ in 0..10 {
        let started = Instant::now();
        history.poll(&tracked, Duration::from_secs(60));
        worst = worst.max(started.elapsed());
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("steady-state poll (worst of 10): {worst:?}; warnings: {:?}", history.warnings);
    assert!(worst < Duration::from_millis(50));
}

#[test]
#[ignore]
fn probe_opencode_store() {
    let path = db_path();
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let mut history = OpenCodeHistory::new(path.clone());
    history.use_servers = false;
    let started = Instant::now();
    let (all, _) = history.poll(&HashSet::new(), Duration::ZERO);
    println!("opencode: {} sessions listed in {:?} from {} ({} GB)", all.len(), started.elapsed(), path.display(), size / 1_000_000_000);
    let mut recent = all.clone();
    recent.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
    let tracked: HashSet<String> = recent.iter().take(6).map(|s| s.id.clone()).collect();
    let started = Instant::now();
    let (changed, _) = history.poll(&tracked, Duration::from_secs(60));
    println!("tracked {} first poll {:?}", tracked.len(), started.elapsed());
    let now = now_ms();
    for s in changed.iter().filter(|s| s.tracked) {
        println!(
            "  {} {:<45} {:?} pending={} finished={} last_event={} agents={} [{}] cwd={} git_root={}",
            s.id,
            s.name.chars().take(45).collect::<String>(),
            s.activity,
            s.pending,
            s.finished_ms.map(|f| age(now, f)).unwrap_or("-".into()),
            age(now, s.last_event_ms),
            s.agents.len(),
            s.agents.iter().map(|a| format!("{}:{:?}", a.name, a.status)).collect::<Vec<_>>().join(", "),
            s.cwd,
            s.git_root,
        );
    }
    let mut worst = Duration::ZERO;
    let mut total = Duration::ZERO;
    for _ in 0..20 {
        let started = Instant::now();
        history.poll(&tracked, Duration::from_secs(60));
        let took = started.elapsed();
        worst = worst.max(took);
        total += took;
        std::thread::sleep(Duration::from_millis(50));
    }
    let started = Instant::now();
    history.poll(&tracked, Duration::ZERO);
    println!("steady-state poll: worst {worst:?}, mean {:?}; full poll {:?}; warnings: {:?}", total / 20, started.elapsed(), history.warnings);
    assert!(worst < Duration::from_millis(50), "a poll must stay cheap");
}

/// A free OpenCode model (`opencode models`), never a paid one.
fn free_model() -> Option<String> {
    let output = super::config::run(&super::opencode::find_cli().ok()?, &["models", "opencode"], &[], Duration::from_secs(60)).ok()?;
    let models: Vec<&str> = output.lines().map(str::trim).filter(|l| l.starts_with("opencode/") && l.ends_with("-free")).collect();
    ["opencode/deepseek-v4.1-flash-free", "opencode/glm-5.3-flash-free"].iter().find(|m| models.contains(m)).map(|m| m.to_string()).or(models.first().map(|m| m.to_string()))
}

#[test]
#[ignore]
fn probe_opencode_e2e() {
    let Some(model) = free_model() else {
        println!("no free OpenCode model: skipped");
        return;
    };
    assert!(model.ends_with("-free"));
    let dir = temp_dir("oc");
    let home = temp_dir("oc-home");
    let plugin = crate::cli::write_plugin(&home).unwrap();
    let integration = Integration { plugin_dir: Some(plugin), env: vec![("COPILOT_CANVAS_SESSION".into(), "probe-oc".into())] };
    let launch = AgentLaunch { agent: "opencode".into(), model: Some(model.clone()), ..Default::default() };
    let command = OpenCode.command("probe-oc", &launch, &integration).unwrap();
    let env = |k: &str| command.env.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone()).unwrap();
    let (port, password): (u16, String) = (env(super::opencode::PORT_ENV).parse().unwrap(), env(super::opencode::PASSWORD_ENV));
    println!("model {model}; {} {}", command.program, command.args.join(" "));
    let launched = now_ms();
    let mut console = Console::start(&command, &dir, 120, 36).unwrap();
    let up = Instant::now();
    let ready = console.wait(Duration::from_secs(60), |_| opencode_server::live(port, &password).is_ok());
    println!("server answered: {ready} after {:?}", up.elapsed());
    assert!(ready, "screen:\n{}", console.text());
    // Basic auth is enforced.
    assert!(opencode_server::get_json(port, "wrong", "/session/status").is_err());
    let config = opencode_server::get_json(port, &password, "/config").unwrap_or_default();
    println!("TUI config: skills.paths={} ; model={}", config["skills"]["paths"], config["model"]);
    console.wait(Duration::from_secs(20), |c| c.text().lines().filter(|l| !l.trim().is_empty()).count() > 5);
    std::thread::sleep(Duration::from_secs(2));
    println!("--- TUI before prompt ---\n{}", console.text());
    let typed_at = now_ms();
    console.type_text("Reply with just OK");
    std::thread::sleep(Duration::from_millis(300));
    console.send(b"\r");
    let mut history = OpenCodeHistory::new(db_path());
    let mut session: Option<String> = None;
    let mut statuses: Vec<(u64, String)> = Vec::new();
    let mut activities: Vec<(u64, Activity, Option<u64>, u16, bool)> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(150);
    let mut done = false;
    while Instant::now() < deadline && !done {
        std::thread::sleep(Duration::from_millis(250));
        let t = now_ms() - typed_at;
        if let Ok(live) = opencode_server::live(port, &password) {
            let state = if live.status.is_empty() { "idle".to_owned() } else { live.status.values().cloned().collect::<Vec<_>>().join(",") };
            if statuses.last().map(|(_, s)| s) != Some(&state) {
                statuses.push((t, state));
            }
        }
        if session.is_none() {
            session = discover_new_session(&dir.display().to_string(), launched.saturating_sub(1_000));
            if let Some(id) = &session {
                println!("discovered {id} after {t} ms; server sessions: {:?}", super::opencode::server_sessions("probe-oc"));
            }
        }
        let Some(id) = &session else { continue };
        let tracked: HashSet<String> = [id.clone()].into_iter().collect();
        let (changed, _) = history.poll(&tracked, Duration::from_secs(600));
        if let Some(s) = changed.iter().find(|s| &s.id == id) {
            activities.push((t, s.activity, s.finished_ms, s.pending, s.live));
            if s.activity == Activity::Idle && s.finished_ms.is_some() {
                done = true;
            }
        }
    }
    println!("server status transitions (ms after Enter): {statuses:?}");
    println!("history transitions (ms, activity, finished_ms, pending, live): {activities:?}");
    println!("titles: {:?}", lock(&console.seen).titles);
    println!("--- TUI after ---\n{}", console.text());
    console.stop();
    if let Some(id) = &session {
        // Leave no probe session behind (OpenCode deletes it itself).
        let deleted = super::config::run(&command.program, &["session", "delete", id], &[], Duration::from_secs(30));
        println!("deleted probe session: {}", deleted.is_ok());
    }
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&home);
    assert!(session.is_some(), "the new session was discovered");
    assert!(activities.iter().any(|a| a.1 == Activity::Working), "seen working");
    assert!(done, "settled idle with a finish time");
}

#[test]
#[ignore]
fn probe_claude_launch() {
    // Never types anything: only checks that the flags start a session and the pid file shows it.
    // Claude runs against its own throw-away config folder (CLAUDE_CONFIG_DIR) that trusts the
    // work folder up front: a fresh folder otherwise stops at the trust dialog (no pid file yet),
    // and the user's real configuration stays untouched.
    let dir = temp_dir("claude");
    let config_dir = temp_dir("claude-config");
    let trusted = dir.display().to_string().replace('\\', "/");
    let seed = serde_json::json!({ "hasCompletedOnboarding": true, "theme": "dark", "projects": { trusted: { "hasTrustDialogAccepted": true } } });
    std::fs::write(config_dir.join(".claude.json"), seed.to_string()).unwrap();
    let home = temp_dir("claude-home");
    let plugin = crate::cli::write_plugin(&home).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let integration = Integration { plugin_dir: Some(plugin.clone()), env: vec![("COPILOT_CANVAS_SESSION".into(), id.clone())] };
    let launch = AgentLaunch { agent: "claude".into(), name: Some("canvas probe".into()), env: vec![("CLAUDE_CONFIG_DIR".into(), config_dir.display().to_string())], ..Default::default() };
    let claude_home = ClaudeHome { dir: config_dir.clone(), user_config: config_dir.join(".claude.json") };
    let command = Claude.command_in(&claude_home, &id, &launch, &integration).unwrap();
    println!("{} {}", command.program, command.args.join(" "));
    let validate = super::config::run(&command.program, &["plugin", "validate", &plugin.display().to_string()], &[], Duration::from_secs(60));
    println!("claude plugin validate: {:?}", validate.map(|o| o.trim().lines().last().unwrap_or_default().to_owned()));
    let mut console = Console::start(&command, &dir, 120, 36).unwrap();
    let pid = console.child.process_id().unwrap_or(0);
    let started = Instant::now();
    let mut statuses = Vec::new();
    let found = console.wait(Duration::from_secs(40), |_| {
        if let Some(pid) = live_sessions(&config_dir).get(&id) {
            let state = format!("{}/{}", pid.status, pid.waiting_for);
            if statuses.last() != Some(&state) {
                statuses.push(state);
            }
            return started.elapsed() > Duration::from_secs(10);
        }
        false
    });
    let pid_file = config_dir.join("sessions").join(format!("{pid}.json"));
    println!("pid file for {id}: {found} ({:?}); statuses {statuses:?}; {} exists: {}", started.elapsed(), pid_file.display(), pid_file.exists());
    if let Ok(text) = std::fs::read_to_string(&pid_file) {
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        println!("pid file: sessionId={} status={} waitingFor={} name={} procStart={}", v["sessionId"], v["status"], v["waitingFor"], v["name"], v["procStart"]);
    }
    let mut history = ClaudeHistory::new(config_dir.clone());
    let (changed, _) = history.poll(&[id.clone()].into_iter().collect(), Duration::ZERO);
    let summary = changed.iter().find(|s| s.id == id).cloned();
    println!("summary: {:?}", summary.as_ref().map(|s| (&s.name, s.live, s.activity, s.pending, &s.cwd)));
    {
        let seen = lock(&console.seen);
        println!("titles: {:?}; progress: {:?}; bytes {}", seen.titles, seen.progress, seen.bytes);
    }
    println!("--- screen ---\n{}", console.text());
    println!("transcript written without a prompt: {}", super::claude_history::find_transcript(&config_dir, &id, None).is_some());
    console.stop();
    std::thread::sleep(Duration::from_secs(1));
    println!("after stop: pid file live = {}", live_sessions(&config_dir).contains_key(&id));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&config_dir);
    assert!(found, "Claude wrote a live pid file for the session id we chose");
    assert!(summary.is_some_and(|s| s.live));
}
