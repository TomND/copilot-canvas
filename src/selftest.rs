//! `doctor` (environment checks) and `selftest` (end-to-end UI test that drives
//! the real client inside a hidden pseudoconsole, using demo shells only).

use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::client::doc::Doc;
use crate::paths::Instance;
use canvas_host::emulator::alacritty::AlacrittyEmulator;
use canvas_host::emulator::{Emulator, feed_collect};
use crate::service::term::{headless, lock, screen_text};

type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// A pseudoconsole whose output feeds an emulator; emulator replies (cursor
/// position reports that ConPTY waits for) are written back automatically.
pub struct Console {
    pub screen: Arc<Mutex<AlacrittyEmulator>>,
    writer: SharedWriter,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl Console {
    pub fn start(program: &Path, args: &[String], cols: u16, rows: u16) -> Result<Self> {
        Self::start_with_env(program, args, &[], cols, rows)
    }

    pub fn start_with_env(program: &Path, args: &[String], env: &[(&str, &Path)], cols: u16, rows: u16) -> Result<Self> {
        Self::start_full(program, args, env, cols, rows, false)
    }

    pub fn start_full(program: &Path, args: &[String], env: &[(&str, &Path)], cols: u16, rows: u16, kitty: bool) -> Result<Self> {
        let pair = native_pty_system().openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
        let mut command = CommandBuilder::new(program);
        command.args(args);
        for (key, value) in env {
            command.env(key, value);
        }
        if let Ok(cwd) = std::env::current_dir() {
            command.cwd(cwd);
        }
        let child = pair.slave.spawn_command(command)?;
        drop(pair.slave);
        let screen = Arc::new(Mutex::new(headless(cols, rows, kitty)));
        let mut reader = pair.master.try_clone_reader()?;
        let writer: SharedWriter = Arc::new(Mutex::new(pair.master.take_writer()?));
        let (shared, reply_writer) = (screen.clone(), writer.clone());
        // Probes: CC_RAW_CAPTURE=<file> records the program's raw output.
        let mut capture = std::env::var_os("CC_RAW_CAPTURE").and_then(|p| std::fs::File::create(p).ok());
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 65536];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if let Some(file) = capture.as_mut() {
                    let _ = file.write_all(&buf[..n]);
                }
                let replies = feed_collect(&mut *lock(&shared), &buf[..n]).replies;
                for reply in replies {
                    let mut w = lock(&reply_writer);
                    let _ = w.write_all(&reply).and_then(|_| w.flush());
                }
            }
        });
        Ok(Self { screen, writer, child, _master: pair.master })
    }

    pub fn text(&self) -> String {
        screen_text(&lock(&self.screen).snapshot())
    }

    pub fn mode(&self) -> String {
        format!("{:?}", lock(&self.screen).term.mode())
    }

    /// Writes input without the settle delay (for load tests).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn send_now(&mut self, bytes: &[u8]) -> Result<()> {
        let mut w = lock(&self.writer);
        w.write_all(bytes)?;
        w.flush()?;
        Ok(())
    }

    pub fn send(&mut self, bytes: &[u8]) -> Result<()> {
        {
            let mut w = lock(&self.writer);
            w.write_all(bytes)?;
            w.flush()?;
        }
        std::thread::sleep(Duration::from_millis(60));
        Ok(())
    }

    pub fn wait(&self, what: &str, timeout: Duration, predicate: impl Fn(&str) -> bool) -> Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if predicate(&self.text()) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        bail!("timed out waiting for {what}\n--- screen ---\n{}", self.text())
    }

    fn click(&mut self, x: u16, y: u16) -> Result<()> {
        self.send(format!("\x1b[<0;{};{}M", x + 1, y + 1).as_bytes())?;
        self.send(format!("\x1b[<0;{};{}m", x + 1, y + 1).as_bytes())
    }

    fn drag(&mut self, button: u8, from: (u16, u16), to: (u16, u16)) -> Result<()> {
        self.send(format!("\x1b[<{button};{};{}M", from.0 + 1, from.1 + 1).as_bytes())?;
        let steps = 4;
        for i in 1..=steps {
            let x = from.0 as i32 + (to.0 as i32 - from.0 as i32) * i / steps;
            let y = from.1 as i32 + (to.1 as i32 - from.1 as i32) * i / steps;
            self.send(format!("\x1b[<{};{};{}M", button + 32, x + 1, y + 1).as_bytes())?;
        }
        self.send(format!("\x1b[<{button};{};{}m", to.0 + 1, to.1 + 1).as_bytes())
    }

    /// Screen cell of the first occurrence of `needle` starting within columns `[min_x, max_x)`.
    pub fn find_in(&self, needle: &str, min_x: u16, max_x: u16) -> Option<(u16, u16)> {
        self.text().lines().enumerate().find_map(|(y, line)| {
            line.match_indices(needle).find_map(|(byte, _)| {
                let x = crate::client::paint::text_width(&line[..byte]) as u16;
                (x >= min_x && x < max_x).then_some((x, y as u16))
            })
        })
    }

    /// Like `find_in`, but an error includes the screen for diagnosis.
    pub fn locate(&self, needle: &str, range: (u16, u16)) -> Result<(u16, u16)> {
        self.find_in(needle, range.0, range.1).with_context(|| format!("{needle:?} not visible in columns {range:?}\n--- screen ---\n{}", self.text()))
    }

    /// Screen cell of the first occurrence of `needle`.
    pub fn find(&self, needle: &str) -> Option<(u16, u16)> {
        self.find_in(needle, 0, u16::MAX)
    }

    pub fn wait_exit(&mut self, timeout: Duration) -> Result<u32> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status.exit_code());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        bail!("the process did not exit\n--- screen ---\n{}", self.text())
    }
}

pub fn doctor(instance: &Instance) -> Result<()> {
    let mut ok = true;
    let mut line = |good: bool, label: &str, detail: String| {
        ok &= good;
        println!("{} {label:<20} {detail}", if good { "✓" } else { "✗" });
    };
    match crate::service::find_on_path("copilot.exe") {
        Some(path) => line(true, "Copilot CLI", path.display().to_string()),
        None => line(false, "Copilot CLI", "copilot.exe not found on PATH".into()),
    }
    match std::process::Command::new("git").arg("--version").output() {
        Ok(out) => line(out.status.success(), "Git", String::from_utf8_lossy(&out.stdout).trim().to_owned()),
        Err(error) => line(false, "Git", error.to_string()),
    }
    let sessions = std::fs::read_dir(&instance.sessions_dir)
        .map(|d| d.filter_map(|e| e.ok()).filter(|e| crate::index::is_session_id(&e.file_name().to_string_lossy())).count())
        .unwrap_or(0);
    line(true, "Session history", format!("{sessions} sessions in {}", instance.sessions_dir.display()));
    let started = Instant::now();
    let probe = (|| -> Result<String> {
        let mut console = Console::start(Path::new("cmd.exe"), &["/d".into(), "/c".into(), "echo CANVAS_CONPTY_OK".into()], 60, 10)?;
        console.wait("ConPTY output", Duration::from_secs(10), |t| t.contains("CANVAS_CONPTY_OK"))?;
        let elapsed = started.elapsed().as_millis();
        let _ = console.wait_exit(Duration::from_secs(5));
        Ok(format!("output rendered in {elapsed} ms"))
    })();
    match probe {
        Ok(detail) => line(true, "ConPTY + emulator", detail),
        Err(error) => line(false, "ConPTY + emulator", format!("{error:#}")),
    }
    match crate::client::conn::try_connect(instance) {
        Ok(conn) => {
            let same = conn.service_build == crate::paths::build_id();
            line(true, "Terminal service", format!("running ({})", if same { "same build" } else { "different build; restart when convenient" }));
        }
        Err(_) => line(true, "Terminal service", "not running (starts automatically)".into()),
    }
    let (workspaces, warnings) = crate::vscode::scan(&crate::vscode::default_roots());
    let copilot: usize = workspaces.iter().map(|w| w.sessions.len()).sum();
    let unreadable = if warnings.is_empty() { String::new() } else { format!(", {} unreadable", warnings.len()) };
    line(true, "VS Code agents", format!("{} workspaces, {copilot} Copilot CLI agent entries{unreadable}", workspaces.len()));
    line(true, "Data directory", instance.home.display().to_string());
    if !ok {
        bail!("some checks failed");
    }
    Ok(())
}

fn load(path: &Path) -> Result<Doc> {
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

fn wait_doc(path: &Path, what: &str, predicate: impl Fn(&Doc) -> bool) -> Result<Doc> {
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if let Ok(doc) = load(path)
            && predicate(&doc)
        {
            return Ok(doc);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("canvas never satisfied: {what}\n{}", std::fs::read_to_string(path).unwrap_or_default())
}

/// Real Copilot check (no prompts are sent): launches a new Copilot session from
/// the canvas, verifies its UI renders, verifies live sessions elsewhere are not
/// duplicated, stops it gracefully, and removes the generated session folder.
pub fn copilot() -> Result<()> {
    let root = std::env::temp_dir().join(format!("copilot-canvas-check-{}", uuid::Uuid::new_v4()));
    let work = root.join("work");
    std::fs::create_dir_all(&work)?;
    let instance = Instance::new(Some(format!("check{}", std::process::id())), Some(root.join("home")), None)?;
    let result = copilot_scenario(&instance, &work);
    if let Ok(conn) = crate::client::conn::try_connect(&instance) {
        let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
        std::thread::sleep(Duration::from_secs(7));
    }
    let cleanup = cleanup_generated(&instance, &root.join("home").join("canvas.json"));
    let _ = std::fs::remove_dir_all(&root);
    result.and(cleanup)
}

fn cleanup_generated(instance: &Instance, canvas: &Path) -> Result<()> {
    let Ok(doc) = load(canvas) else { return Ok(()) };
    let Some(node) = doc.nodes.iter().find(|n| n.name == "Canvas copilot check") else { return Ok(()) };
    let dir = instance.sessions_dir.join(&node.id);
    if !dir.is_dir() {
        return Ok(());
    }
    let meta = std::fs::read_to_string(dir.join("workspace.yaml")).unwrap_or_default();
    let events = std::fs::read_to_string(dir.join("events.jsonl")).unwrap_or_default();
    anyhow::ensure!(meta.contains(&node.id), "refusing to delete {}: metadata does not match", dir.display());
    anyhow::ensure!(!events.contains("\"user.message\""), "a prompt was recorded in {}; not deleting", dir.display());
    for _ in 0..20 {
        if !crate::service::session_live(&instance.sessions_dir, &node.id) && std::fs::remove_dir_all(&dir).is_ok() {
            println!("  · removed the generated test session {}", &node.id[..8]);
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    bail!("could not remove the generated test session {}", dir.display())
}

fn copilot_scenario(instance: &Instance, work: &Path) -> Result<()> {
    let exe = std::env::current_exe()?;
    let args: Vec<String> = vec![
        "--instance".into(),
        instance.name.clone(),
        "--home".into(),
        instance.home.to_string_lossy().into_owned(),
        "--cwd".into(),
        work.to_string_lossy().into_owned(),
    ];
    println!("Copilot Canvas real-Copilot check (no prompts are sent)");
    let mut ui = Console::start(&exe, &args, 170, 46)?;
    step("recent sessions are indexed and placed on a fresh canvas");
    ui.wait("auto-import", Duration::from_secs(30), |t| t.contains("8 sessions"))?;
    step("a session that is live elsewhere is shown as such and never duplicated");
    let deadline = Instant::now() + Duration::from_secs(10);
    while ui.find_in("↗", SIDEBAR.0, SIDEBAR.1).is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    if let Some((x, y)) = ui.find_in("↗", SIDEBAR.0, SIDEBAR.1) {
        ui.click(x + 2, y)?;
        ui.wait("refusal notice", Duration::from_secs(5), |t| t.contains("never starts a second copy"))?;
    } else {
        println!("    (no session is currently open elsewhere; skipped)");
    }
    step("VS Code sync adds agent sessions active in the last 48 h, grouped like the Agents window (read-only)");
    ui.send(b"v")?;
    ui.wait("sync summary", Duration::from_secs(60), |t| t.contains("VS Code sync: added") || t.contains("No VS Code"))?;
    let summary = ui.text().lines().find(|l| l.contains("VS Code sync: added")).map(|l| l.split("  ").next().unwrap_or(l).trim().to_owned());
    let added: usize = summary.as_deref().and_then(|s| s.split_whitespace().nth(4)).and_then(|n| n.parse().ok()).unwrap_or(0);
    if let Some(summary) = &summary {
        println!("    {summary}");
    }
    let canvas = instance.home.join("canvas.json");
    let doc = wait_doc(&canvas, "synced sessions saved", |d| d.nodes.iter().filter(|n| n.source.as_deref() == Some("vscode")).count() >= added)?;
    let groups = doc.groups.iter().filter(|g| g.source.as_deref().is_some_and(|s| s.starts_with("vscode-"))).count();
    let synced = doc.nodes.iter().filter(|n| n.source.as_deref() == Some("vscode")).count();
    println!("    {synced} VS Code sessions on the canvas in {groups} VS Code group(s)");
    for group in doc.groups.iter().filter(|g| g.source.as_deref().is_some_and(|s| s.starts_with("vscode-"))) {
        let members = doc.members(&group.id);
        let subs = members.iter().filter_map(|m| doc.node(m)).filter(|n| n.kind == crate::client::doc::NodeKind::Session && n.parent.is_some()).count();
        let agents = group.source.as_deref().is_some_and(|s| s.starts_with("vscode-group:"));
        println!("      {}{} · {} sessions ({subs} sub-sessions)", group.name, if agents { " [Agents-window group]" } else { "" }, members.iter().filter_map(|m| doc.node(m)).filter(|n| n.kind == crate::client::doc::NodeKind::Session).count());
    }
    step("a second sync is idempotent (nothing added, renamed, linked or moved)");
    ui.send(b"v")?;
    ui.wait("second sync", Duration::from_secs(60), |t| t.contains("VS Code sync: added 0 session(s)"))?;
    let again = ui.text().lines().find(|l| l.contains("VS Code sync: added 0")).unwrap_or_default().to_owned();
    anyhow::ensure!(!again.contains("renamed") && !again.contains("links") && !again.contains("moved"), "second sync changed boxes: {again}");
    step("plugins / MCP / skills views load from the Copilot CLI");
    ui.send(b"\x1b3")?;
    ui.wait("plugins view", Duration::from_secs(60), |t| t.contains("Install plugin"))?;
    ui.send(b"\x1b4")?;
    ui.wait("mcp view", Duration::from_secs(10), |t| t.contains("Add MCP server"))?;
    ui.send(b"\x1b5")?;
    ui.wait("skills view", Duration::from_secs(10), |t| t.contains("SKILLS") && (t.contains("Add skill") || t.contains(" of ")))?;
    ui.send(b"\x1b7")?;
    ui.wait("vs code view", Duration::from_secs(5), |t| t.contains("Migrate skills"))?;
    step("migration preview lists VS Code items without changing anything");
    let (mx, my) = ui.locate("Migrate skills", SIDEBAR)?;
    ui.click(mx, my)?;
    ui.wait("migration preview", Duration::from_secs(60), |t| t.contains("selected · Space toggle") || t.contains("Nothing to migrate"))?;
    let preview = ui.text();
    let count = |mark: &str| preview.lines().filter(|l| l.contains(mark)).count();
    println!("    preview: {} new, {} already in Copilot, {} need attention", count("[x]") + count("[ ]"), count("[=]"), count("[!]") + count("[?]"));
    ui.send(b"\x1b")?;
    ui.wait("preview closed", Duration::from_secs(5), |t| !t.contains("Space toggle · Enter apply"))?;
    ui.send(b"\x1b1")?;
    step("new Copilot session from the dialog renders the real Copilot UI");
    ui.send(b"\x1d")?;
    let focus = ui.text();
    if focus.contains("Typing goes to the terminal") {
        ui.send(b"\x1d")?;
    }
    ui.send(b"n")?;
    ui.wait("new session dialog", Duration::from_secs(5), |t| t.contains("New session"))?;
    ui.send(b"Canvas copilot check\r")?;
    ui.wait("Copilot UI", Duration::from_secs(90), |t| {
        t.contains("Canvas copilot check · ") && (t.contains("Copilot v") || t.contains("trust") || t.contains("Trust") || t.contains("❯"))
    })?;
    step("new sessions start with --allow-all (Settings → Permissions default)");
    let log = std::fs::read_to_string(instance.home.join(format!("service-{}.log", instance.name))).unwrap_or_default();
    let pid = log.lines().rev().find_map(|l| l.split(" pid ").nth(1)).and_then(|p| p.trim().parse::<u32>().ok()).context("launched pid in the service log")?;
    let query = format!("(Get-CimInstance Win32_Process -Filter 'ProcessId={pid}').CommandLine");
    let out = std::process::Command::new("powershell.exe").args(["-NoProfile", "-Command", &query]).output()?;
    let command_line = String::from_utf8_lossy(&out.stdout).to_string();
    anyhow::ensure!(command_line.contains("--allow-all"), "Copilot was launched without --allow-all: {command_line}");
    step("typing reaches Copilot's input (not submitted)");
    if !(ui.text().contains("trust") || ui.text().contains("Trust")) {
        ui.send(b"canvas typing probe")?;
        ui.wait("typed text echoed by Copilot", Duration::from_secs(15), |t| t.contains("canvas typing probe"))?;
        ui.send(b"\x15")?;
        step("mouse wheel over the terminal scrolls Copilot's timeline (local ! command, no prompt)");
        ui.send(b"!1..120 | % ToString 'canvasrow000'")?;
        std::thread::sleep(Duration::from_millis(400));
        ui.send(b"\r")?;
        ui.wait("command output", Duration::from_secs(30), |t| t.contains("canvasrow120"))?;
        std::thread::sleep(Duration::from_millis(800));
        let lowest = |text: &str| -> Option<u32> {
            text.lines().filter_map(|l| l.split("canvasrow").nth(1)).filter_map(|r| r.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()).min()
        };
        let before = lowest(&ui.text()).context("visible output")?;
        let (x, y) = ui.find("canvasrow120").context("output position")?;
        for notch in 0..3 {
            ui.send(format!("\x1b[<64;{};{}M", x + 2, y + 1).as_bytes())?;
            std::thread::sleep(Duration::from_millis(500));
            println!("    after notch {}: first visible line {:?}", notch + 1, lowest(&ui.text()));
        }
        ui.wait("earlier lines after wheel up", Duration::from_secs(10), |t| lowest(t).is_some_and(|n| n < before))?;
        println!("    first visible line went from {before} to {}", lowest(&ui.text()).unwrap_or(0));
    }
    step("stop all and quit (Copilot exits gracefully)");
    ui.send(b"\x1d")?;
    ui.send(b"q")?;
    ui.wait("quit dialog", Duration::from_secs(5), |t| t.contains("Stop all and quit"))?;
    ui.send(b"\x1b[C\r")?;
    ui.wait_exit(Duration::from_secs(15))?;
    println!("PASS");
    Ok(())
}

/// Canvas columns at 170 columns with the default 30-column sidebar and 55% terminals.
const CANVAS: (u16, u16) = (34, 95);
/// Sidebar columns (right of the 3-column activity bar).
const SIDEBAR: (u16, u16) = (3, 33);

fn step(name: &str) {
    println!("  · {name}");
}

/// `remote`: `Some("loopback")` runs the same scenario with the client
/// connected over TCP (`--remote loopback`).
pub fn run(remote: Option<&str>) -> Result<()> {
    let root = std::env::temp_dir().join(format!("copilot-canvas-selftest-{}", uuid::Uuid::new_v4()));
    let home = root.join("home");
    let sessions = root.join("sessions");
    let work = root.join("work");
    std::fs::create_dir_all(&sessions)?;
    std::fs::create_dir_all(&work)?;
    let instance = Instance::new(Some(format!("selftest{}", std::process::id())), Some(home), Some(sessions))?;
    let result = scenario(&instance, &work, remote);
    // Always stop the isolated test service and its shells.
    if let Ok(conn) = crate::client::conn::try_connect(&instance) {
        let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
        std::thread::sleep(Duration::from_millis(2500));
    }
    match &result {
        Ok(()) => {
            for _ in 0..10 {
                if std::fs::remove_dir_all(&root).is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        }
        Err(_) => eprintln!("artifacts kept in {}", root.display()),
    }
    result
}

fn scenario(instance: &Instance, work: &Path, remote: Option<&str>) -> Result<()> {
    let canvas = instance.home.join("demo-canvas.json");
    let exe = std::env::current_exe()?;
    let mut args: Vec<String> = vec![
        "--demo".into(),
        "--instance".into(),
        instance.name.clone(),
        "--home".into(),
        instance.home.to_string_lossy().into_owned(),
        "--sessions-dir".into(),
        instance.sessions_dir.to_string_lossy().into_owned(),
        "--cwd".into(),
        work.to_string_lossy().into_owned(),
    ];
    if let Some(remote) = remote {
        args.extend(["--remote".into(), remote.to_owned()]);
    }
    let over = remote.map(|r| format!(", client over --remote {r}")).unwrap_or_default();
    println!("Copilot Canvas self-test (demo shells only; isolated data in {}{over})", instance.home.display());
    let copilot_home = instance.home.join("copilot-home");
    std::fs::create_dir_all(&copilot_home)?;
    let automations_db = copilot_home.join("data.db");
    crate::automations::write_sample_db(&automations_db)?;
    let env = [("COPILOT_HOME", copilot_home.as_path())];
    let started = Instant::now();
    let mut ui = Console::start_with_env(&exe, &args, &env, 170, 46)?;
    step("client starts and auto-starts the terminal service");
    ui.wait("the canvas", Duration::from_secs(20), |t| t.contains("Copilot Canvas") && t.contains("Build session canvas"))?;
    let startup = started.elapsed();
    if remote.is_some() {
        // The client really talks TCP: the service published its port and logged the connection.
        anyhow::ensure!(crate::client::conn::listening_port(instance).is_some(), "the service publishes its TCP port");
        let log = std::fs::read_to_string(instance.log_path()).unwrap_or_default();
        anyhow::ensure!(log.contains("connected (tui, protocol 8"), "the service saw a remote TUI client:\n{log}");
    }
    if std::env::var_os("CANVAS_SELFTEST_DEBUG").is_some() {
        println!("terminal modes requested by the client console: {}", ui.mode());
        let (hx, hy) = ui.find(" ? ").context("help button")?;
        ui.click(hx + 1, hy)?;
        std::thread::sleep(Duration::from_millis(500));
        println!("after clicking help: help visible = {}", ui.text().contains("Copilot Canvas — help"));
        ui.send(b"\x1b")?;
    }

    step("sidebar lists the group and its sessions");
    ui.wait("sidebar tree", Duration::from_secs(5), |t| t.contains("Canvas project") && t.contains("Review implementation"))?;

    step("Enter opens a real native terminal for the selected session");
    ui.send(b"\r")?;
    ui.wait("PowerShell prompt", Duration::from_secs(20), |t| t.contains("PS "))?;
    ui.send(b"Write-Output ('CANVAS_' + 'TERMINAL_OK')\r")?;
    ui.wait("command output", Duration::from_secs(10), |t| t.contains("CANVAS_TERMINAL_OK"))?;
    step("the active terminal's box gets a halo on the canvas (heavy while typing)");
    let (bx, by) = ui.locate("Build session canvas", CANVAS)?;
    let halo_row = ui.text().lines().nth(by as usize - 1).unwrap_or_default().chars().skip(bx.saturating_sub(6) as usize).take(12).collect::<String>();
    anyhow::ensure!(halo_row.contains('━') || halo_row.contains('┏'), "no halo above the active box: {halo_row:?}");

    step("Ctrl+] returns keyboard focus to the canvas");
    ui.send(b"\x1d")?;
    ui.wait("canvas focus hints", Duration::from_secs(5), |t| t.contains("empty-drag pan"))?;

    step("mouse drag moves a session together with its subagent");
    let before = wait_doc(&canvas, "initial save", |d| d.node("demo-main").is_some())?;
    let main = before.node("demo-main").context("demo node")?.clone();
    let agent = before.node("demo-main:explore").context("demo agent")?.clone();
    let (x, y) = ui.locate("Build session canvas", CANVAS)?;
    ui.drag(0, (x + 2, y + 1), (x + 8, y + 3))?;
    wait_doc(&canvas, "node and subagent moved by (6,2)", |d| {
        let (n, a) = (d.node("demo-main").unwrap(), d.node("demo-main:explore").unwrap());
        (n.x - main.x, n.y - main.y) == (6, 2) && (a.x - agent.x, a.y - agent.y) == (6, 2)
    })?;

    step("rename from the keyboard");
    ui.send(b"r")?;
    ui.wait("rename dialog", Duration::from_secs(5), |t| t.contains("Rename (canvas label)"))?;
    ui.send(b"\x15Renamed by selftest\r")?;
    wait_doc(&canvas, "rename saved", |d| d.node("demo-main").is_some_and(|n| n.name == "Renamed by selftest"))?;

    step("right-click opens a context menu; toolbar zoom buttons change zoom");
    let (x, y) = ui.locate("Renamed", CANVAS)?;
    ui.send(format!("\x1b[<2;{};{}M\x1b[<2;{};{}m", x + 3, y + 2, x + 3, y + 2).as_bytes())?;
    ui.wait("node context menu", Duration::from_secs(5), |t| t.contains("New subsession") && t.contains("Remove from canvas"))?;
    ui.send(b"\x1b")?;
    ui.wait("menu closed", Duration::from_secs(5), |t| !t.contains("Remove from canvas"))?;
    let (zx, zy) = ui.locate(" − ", (60, 120))?;
    ui.click(zx + 1, zy)?;
    ui.wait("zoomed out", Duration::from_secs(5), |t| t.contains("zoom 50%"))?;
    let (zx, zy) = ui.locate(" + ", (60, 120))?;
    ui.click(zx + 1, zy)?;
    ui.wait("zoomed in", Duration::from_secs(5), |t| t.contains("zoom 100%"))?;

    step("new session dialog: folder prefilled, Enter creates and launches");
    ui.send(b"n")?;
    ui.wait("new session dialog", Duration::from_secs(5), |t| t.contains("New session") && t.contains("Folder"))?;
    ui.wait("git probe", Duration::from_secs(10), |t| t.contains("Not a Git repository") || t.contains("Git:"))?;
    ui.send(b"Spawned session\r")?;
    let doc = wait_doc(&canvas, "spawned node", |d| d.nodes.iter().any(|n| n.name == "Spawned session"))?;
    let spawned = doc.nodes.iter().find(|n| n.name == "Spawned session").context("spawned")?.id.clone();
    ui.wait("spawned terminal", Duration::from_secs(20), |t| t.contains("Spawned session · ") && t.contains("PS "))?;

    step("subsession linked to its parent");
    ui.send(b"\x1d")?;
    ui.send(b"s")?;
    ui.wait("subsession dialog", Duration::from_secs(5), |t| t.contains("New subsession of"))?;
    ui.send(b"Child task\r")?;
    wait_doc(&canvas, "subsession parent link", |d| d.nodes.iter().any(|n| n.name == "Child task" && n.parent.as_deref() == Some(spawned.as_str())))?;

    step("right-drag rubber-band selection, then G groups the selection");
    ui.send(b"\x1d")?;
    ui.send(b"\x1b")?;
    ui.send(b"f")?;
    ui.wait("child visible", Duration::from_secs(5), |t| t.contains("Child task"))?;
    let (sx, sy) = ui.locate("Spawn", CANVAS)?;
    let (cx, cy) = ui.locate("Chil", CANVAS)?;
    let from =  (sx.min(cx).saturating_sub(3).max(CANVAS.0), sy.min(cy).saturating_sub(1));
    let to = (sx.max(cx) + 8, sy.max(cy) + 2);
    ui.drag(2, from, to)?;
    ui.wait("multi-selection notice", Duration::from_secs(5), |t| t.contains("sessions selected"))?;
    ui.send(b"g")?;
    ui.wait("group rename", Duration::from_secs(5), |t| t.contains("Rename group"))?;
    ui.send(b"\x15Selftest group\r")?;
    wait_doc(&canvas, "group around both sessions", |d| d.groups.iter().any(|g| g.name == "Selftest group" && d.members(&g.id).len() >= 2))?;

    step("a new group asks for its folder; Enter creates ai-docs, human-outputs and group instructions");
    ui.wait("group folder prompt", Duration::from_secs(5), |t| t.contains("Folder for “Selftest group”"))?;
    ui.send(b"\r")?;
    let doc = wait_doc(&canvas, "group folder saved", |d| d.groups.iter().any(|g| g.name == "Selftest group" && g.folder.is_some()))?;
    let group = doc.groups.iter().find(|g| g.name == "Selftest group").context("group")?;
    let folder = std::path::PathBuf::from(group.folder.clone().unwrap_or_default());
    anyhow::ensure!(folder.starts_with(instance.home.join("groups")), "suggested folder under the canvas home: {}", folder.display());
    for sub in ["ai-docs", "human-outputs", r".canvas\canvas-group.instructions.md"] {
        anyhow::ensure!(folder.join(sub).exists(), "{} exists", folder.join(sub).display());
    }
    ui.wait("group panel folder actions", Duration::from_secs(5), |t| t.contains("Open AI docs") && t.contains("Open human outputs"))?;

    step("tiled view shows both live shells");
    ui.send(b"t")?;
    ui.wait("tiled", Duration::from_secs(5), |t| t.contains("Tiled") && t.contains("CANVAS_TERMINAL_OK") && t.contains("Spawned session · "))?;

    step("sidebar click reveals and opens a session");
    let (rx, ry) = ui.locate("Renamed by selftest", SIDEBAR)?;
    ui.click(rx, ry)?;
    ui.wait("focus on terminal", Duration::from_secs(5), |t| t.contains("Typing goes to the terminal"))?;

    step("links in terminals: plain click focuses, Ctrl+click opens (missing path → notice, nothing launched)");
    ui.send(b"Write-Output ('probe ' + 'C:\\canvas-link-probe\\missing.txt' + ' end')\r")?;
    ui.wait("path printed", Duration::from_secs(10), |t| t.contains("probe C:\\canvas-link"))?;
    let (lx, ly) = ui.find("probe C:\\canvas-link-probe").context("printed path")?;
    let (lx, ly) = (lx + 10, ly + 1);
    ui.send(format!("\x1b[<0;{lx};{ly}M\x1b[<0;{lx};{ly}m").as_bytes())?;
    std::thread::sleep(Duration::from_millis(600));
    anyhow::ensure!(!ui.text().contains("does not exist"), "plain click must not open terminal links");
    ui.send(format!("\x1b[<16;{lx};{ly}M\x1b[<16;{lx};{ly}m").as_bytes())?;
    ui.wait("Ctrl+click resolved the link", Duration::from_secs(5), |t| t.contains("missing.txt does not exist"))?;

    step("activity bar: Automations view lists Copilot automations and adds them to an Automations group");
    ui.send(b"\x1d")?;
    ui.send(b"\x1b2")?;
    ui.wait("automations list", Duration::from_secs(10), |t| t.contains("Morning triage") && t.contains("Cron job"))?;
    wait_doc(&canvas, "automation boxes", |d| {
        d.groups.iter().any(|g| g.is_automations()) && d.nodes.iter().filter(|n| n.kind == crate::client::doc::NodeKind::Automation).count() == 3
    })?;

    step("repeating automations get the animated loopback (enabled ones only)");
    ui.send(b"\x1b1")?;
    ui.wait("loop markers in the sidebar", Duration::from_secs(10), |t| t.contains("↻ Morning triage") && t.contains("↻ Watch builds"))?;
    anyhow::ensure!(!ui.text().contains("↻ Cron job"), "a disabled automation does not loop");
    anyhow::ensure!(ui.text().contains("↻"), "zoomed out: the ↻ icon marks looping boxes");
    for _ in 0..2 {
        let (zx, zy) = ui.find(" −   +   ?").context("zoom buttons")?;
        ui.click(zx + 5, zy)?;
    }
    ui.wait("zoom 100%", Duration::from_secs(5), |t| t.contains("zoom 100%"))?;
    let (mx, my) = ui.locate("Morning triage", SIDEBAR)?;
    ui.click(mx, my)?;
    ui.wait("loopback drawn beside the box", Duration::from_secs(5), |t| t.contains("↻ daily") && t.contains("◂"))?;
    // The lit dot moves along the arc between frames.
    // The 4×3 cells of the arc next to the box (bottom row ends in ◂).
    let arc = |text: &str| -> String {
        let lines: Vec<Vec<char>> = text.lines().map(|l| l.chars().collect()).collect();
        let Some((row, col)) = lines.iter().enumerate().find_map(|(r, l)| l.iter().position(|c| *c == '◂').map(|c| (r, c))) else { return String::new() };
        (row.saturating_sub(3)..=row).map(|r| lines[r].iter().skip(col).take(3).collect::<String>()).collect::<Vec<_>>().join("|")
    };
    let first = arc(&ui.text());
    let mut moved = false;
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(100));
        if arc(&ui.text()) != first {
            moved = true;
            break;
        }
    }
    anyhow::ensure!(moved, "the loop dot did not move: {first:?}");
    ui.send(b"\x1b2")?;
    ui.wait("automations list again", Duration::from_secs(10), |t| t.contains("Morning triage") && t.contains("Cron job"))?;

    step("clicking an automation shows its prompt, schedule and runs; Space toggles it in Copilot's database");
    let (ax, ay) = ui.locate("Morning triage", SIDEBAR)?;
    ui.click(ax, ay)?;
    ui.wait("automation details", Duration::from_secs(5), |t| t.contains("Summarize new bugs") && t.contains("daily at 08:30") && t.contains("boom"))?;
    let enabled = |key: &str| -> Result<bool> {
        Ok(crate::automations::load(&automations_db)?.iter().find(|a| a.key == key).context("automation")?.enabled)
    };
    ui.send(b" ")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while enabled("wf:w1")? && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::ensure!(!enabled("wf:w1")?, "Space disabled the automation");
    ui.send(b" ")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !enabled("wf:w1")? && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::ensure!(enabled("wf:w1")?, "Space re-enabled the automation\n{}", ui.text());

    step("Settings view edits Copilot launch defaults");
    ui.send(b"\x1b8")?;
    ui.wait("settings view", Duration::from_secs(5), |t| t.contains("New-session defaults") && t.contains("Reasoning effort"))?;
    let (mx, my) = ui.locate("Model", SIDEBAR)?;
    ui.click(mx, my)?;
    ui.wait("model prompt", Duration::from_secs(5), |t| t.contains("model id"))?;
    ui.send(b"\x15claude-selftest\r")?;
    wait_doc(&canvas, "default model saved", |d| d.defaults.model.as_deref() == Some("claude-selftest"))?;

    step("VS Code view offers sync and migration");
    ui.send(b"\x1b7")?;
    ui.wait("vs code view", Duration::from_secs(5), |t| t.contains("Sync sessions now") && t.contains("Migrate skills"))?;
    ui.send(b"\x1b1")?;
    ui.wait("sessions view", Duration::from_secs(5), |t| t.contains("Canvas project"))?;
    let (rx, ry) = ui.locate("Renamed by selftest", SIDEBAR)?;
    ui.click(rx, ry)?;
    ui.wait("focus on terminal", Duration::from_secs(5), |t| t.contains("Typing goes to the terminal"))?;

    step("detach: the client exits while sessions keep running");
    ui.send(b"\x1d")?;
    ui.send(b"q")?;
    ui.wait("quit dialog", Duration::from_secs(5), |t| t.contains("Detach (keep running)"))?;
    ui.send(b"\r")?;
    let code = ui.wait_exit(Duration::from_secs(10))?;
    anyhow::ensure!(code == 0, "client exited with {code}");

    step("reattach: a new client shows the same live terminal and its output");
    let mut ui = Console::start_with_env(&exe, &args, &env, 170, 46)?;
    ui.wait("canvas again", Duration::from_secs(20), |t| t.contains("Renamed by selftest"))?;
    ui.wait("restored tiles with previous output", Duration::from_secs(10), |t| t.contains("CANVAS_TERMINAL_OK") && t.contains("Spawned session · "))?;
    let (x, y) = ui.locate("Renam", CANVAS)?;
    ui.click(x, y)?;
    ui.wait("terminal focus after canvas click", Duration::from_secs(5), |t| t.contains("Typing goes to the terminal"))?;

    step("a session's progress (OSC 9;4), title and bell reach the canvas");
    ui.send(b"$e=[char]27; Write-Host -NoNewline \"$e]9;4;3;0$([char]7)$e]0;Canvas intent$([char]7)\"\r")?;
    ui.wait("working from progress + intent in the pane header", Duration::from_secs(10), |t| {
        t.lines().any(|l| l.contains("Renamed by selftest · working · Canvas intent"))
    })?;
    ui.send(b"Write-Host -NoNewline \"$e]9;4;0;0$([char]7)\"\r")?;
    ui.wait("progress cleared", Duration::from_secs(10), |t| t.lines().any(|l| l.contains("Renamed by selftest · running · Canvas intent")))?;
    step("a Copilot session started inside a session's terminal appears as its linked subsession");
    {
        // A stand-in copilot.exe (a copy of ping) holding a session's in-use lock.
        let fake = work.join("fake-copilot");
        std::fs::create_dir_all(&fake)?;
        std::fs::copy(r"C:\Windows\System32\PING.EXE", fake.join("copilot.exe"))?;
        let child = uuid::Uuid::new_v4().to_string();
        let dir = instance.sessions_dir.join(&child);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("workspace.yaml"), format!("id: {child}\nname: Spawned tracker\ncwd: {}\n", work.display()))?;
        let command = format!(
            "$p = Start-Process -PassThru -NoNewWindow '{}' -ArgumentList '-n','40','127.0.0.1' -RedirectStandardOutput '{}'; New-Item -ItemType File \"{}\\inuse.$($p.Id).lock\" | Out-Null\r",
            fake.join("copilot.exe").display(),
            fake.join("ping.txt").display(),
            dir.display()
        );
        ui.send(command.as_bytes())?;
        let doc = wait_doc(&canvas, "spawned subsession linked to its parent", |d| {
            d.nodes.iter().any(|n| n.id == child && n.parent.as_deref() == Some("demo-main") && n.name == "Spawned tracker")
        })?;
        anyhow::ensure!(doc.nodes.iter().filter(|n| n.id == child).count() == 1, "added once");
        ui.wait("subsession box on the canvas", Duration::from_secs(5), |t| t.contains("Spawned tracker") || t.contains("Spawned"))?;
    }

    step("folder browser + Git detection + new worktree session");
    let repo = work.join("repo");
    std::fs::create_dir_all(&repo)?;
    let git = |args: &[&str]| -> Result<()> {
        let status = std::process::Command::new("git").arg("-C").arg(&repo).args(args).output()?;
        anyhow::ensure!(status.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&status.stderr));
        Ok(())
    };
    git(&["init", "-q", "-b", "main"])?;
    git(&["-c", "user.name=selftest", "-c", "user.email=selftest@example.invalid", "commit", "-q", "--allow-empty", "-m", "init"])?;
    ui.send(b"\x1d")?;
    ui.wait("canvas focus", Duration::from_secs(5), |t| t.contains("empty-drag pan"))?;
    ui.send(b"n")?;
    ui.wait("new session dialog", Duration::from_secs(5), |t| t.contains("New session"))?;
    ui.send(b"Worktree session")?;
    ui.send(b"\t")?;
    ui.send(b"\x15")?;
    ui.send(work.to_string_lossy().as_bytes())?;
    ui.send(b"\t")?;
    ui.send(b"\r")?;
    ui.wait("folder browser", Duration::from_secs(5), |t| t.contains("Choose a folder") && t.contains("repo"))?;
    ui.send(b"repo")?;
    ui.send(b"\r")?;
    ui.wait("inside repo", Duration::from_secs(5), |t| t.contains("\\repo") && t.contains("↑ .."))?;
    ui.send(b"\x13")?;
    ui.wait("Git detected", Duration::from_secs(10), |t| t.contains("Git:") && t.contains("on main"))?;
    for _ in 0..4 {
        ui.send(b"\t")?;
    }
    ui.send(b" ")?;
    ui.wait("worktree options", Duration::from_secs(5), |t| t.contains("Base branch") && t.contains("worktree-session"))?;
    ui.send(b"\t")?;
    ui.send(b"\t")?;
    ui.send(b"\r")?;
    let doc = wait_doc(&canvas, "worktree session node", |d| {
        d.nodes.iter().any(|n| n.name == "Worktree session" && n.cwd.as_deref().is_some_and(|c| c.ends_with(r"repo.worktrees\worktree-session")))
    })?;
    let expected = std::path::PathBuf::from(doc.nodes.iter().find(|n| n.name == "Worktree session").and_then(|n| n.cwd.clone()).unwrap_or_default());
    // The prompt's end is enough: a narrow tile wraps the long temp path
    // (its length depends on the user name), so the full path may span rows.
    ui.wait("shell running inside the worktree", Duration::from_secs(30), |t| {
        t.contains(r"repo.worktrees\worktree-session") || t.contains(r"\worktree-session>")
    })?;
    let branches = std::process::Command::new("git").arg("-C").arg(&expected).args(["branch", "--show-current"]).output()?;
    anyhow::ensure!(String::from_utf8_lossy(&branches.stdout).trim() == "worktree-session", "worktree is on the new branch");

    step("stop all and quit");
    ui.send(b"\x1d")?;
    ui.send(b"q")?;
    ui.wait("quit dialog", Duration::from_secs(5), |t| t.contains("Stop all and quit"))?;
    ui.send(b"\x1b[C\r")?;
    let code = ui.wait_exit(Duration::from_secs(10))?;
    anyhow::ensure!(code == 0, "client exited with {code}");
    let doc = load(&canvas)?;
    anyhow::ensure!(doc.groups.len() >= 2, "both groups persisted");
    println!(
        "PASS in {:.1}s (first paint incl. service start: {} ms)",
        started.elapsed().as_secs_f64(),
        startup.as_millis()
    );
    Ok(())
}

#[cfg(test)]
mod perf_probe {
    use std::time::Duration;
    /// Manual: `PERF_CANVAS=<canvas.json copy> cargo test --release perf_probe -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn perf_probe() {
        let root = std::env::temp_dir().join(format!("cc-perf-{}", uuid::Uuid::new_v4()));
        let (home, sessions, work) = (root.join("home"), root.join("sessions"), root.join("work"));
        for d in [&sessions, &work] {
            std::fs::create_dir_all(d).unwrap();
        }
        let instance = crate::paths::Instance::new(Some(format!("perf{}", std::process::id())), Some(home), Some(sessions)).unwrap();
        let exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"target\release\copilot-canvas.exe");
        let canvas = root.join("canvas.json");
        std::fs::copy(std::env::var("PERF_CANVAS").unwrap(), &canvas).unwrap();
        let args: Vec<String> = vec![
            "--perf".into(), "--instance".into(), instance.name.clone(), "--home".into(), instance.home.to_string_lossy().into_owned(),
            "--sessions-dir".into(), instance.sessions_dir.to_string_lossy().into_owned(), "--cwd".into(), work.to_string_lossy().into_owned(),
            "--canvas".into(), canvas.to_string_lossy().into_owned(),
        ];
        let (cols, rows) = (200, 55);
        let mut ui = super::Console::start(&exe, &args, cols, rows).unwrap();
        let hud = |ui: &super::Console| ui.text().lines().last().unwrap_or_default().rsplit("  ").find(|s| s.contains("fps")).unwrap_or("").trim().to_owned();
        ui.wait("canvas", Duration::from_secs(20), |t| t.contains("Copilot Canvas") && t.contains("fps")).unwrap();
        std::thread::sleep(Duration::from_secs(2));
        for zoom in ["100%", "50%", "25%"] {
            // Continuous middle-button pan, 125 mouse reports per second, as a fast hand would.
            let (x0, y0) = (90, 25);
            ui.send_now(format!("\x1b[<1;{x0};{y0}M").as_bytes()).unwrap();
            let started = std::time::Instant::now();
            let mut i = 0i32;
            while started.elapsed() < Duration::from_secs(3) {
                i += 1;
                let dx = ((i / 4) % 40) - 20;
                ui.send_now(format!("\x1b[<33;{};{}M", x0 + dx, y0 + (i / 9) % 6).as_bytes()).unwrap();
                std::thread::sleep(Duration::from_millis(8));
            }
            ui.send_now(format!("\x1b[<1;{x0};{y0}m").as_bytes()).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            println!("PAN at zoom {zoom}: {}", hud(&ui));
            ui.send_now(b"-").unwrap();
            std::thread::sleep(Duration::from_millis(500));
        }
        ui.send(b"q").unwrap();
        std::thread::sleep(Duration::from_millis(500));
        ui.send(b"\x1b[C\r").unwrap();
        let _ = ui.wait_exit(Duration::from_secs(10));
        if let Ok(conn) = crate::client::conn::try_connect(&instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_secs(2));
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod stress_probe {
    use std::io::Write;
    use std::time::{Duration, Instant};

    use crate::client::doc::{Doc, Group, Node, NodeKind};

    /// Manual: `cargo test --release stress_probe -- --ignored --nocapture`.
    /// 100 live sessions (30 of them busy: streaming events, spawning subagents,
    /// asking permission) while the canvas pans; prints the --perf numbers.
    #[test]
    #[ignore]
    fn stress_probe() {
        let root = std::env::temp_dir().join(format!("cc-stress-{}", uuid::Uuid::new_v4()));
        let (home, sessions, work) = (root.join("home"), root.join("sessions"), root.join("work"));
        for d in [&home, &sessions, &work] {
            std::fs::create_dir_all(d).unwrap();
        }
        let pid = std::process::id();
        let mut doc = Doc::default();
        let n_sessions: usize = std::env::var("STRESS_SESSIONS").ok().and_then(|v| v.parse().ok()).unwrap_or(100);
        let n_agents: usize = std::env::var("STRESS_AGENTS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        let ids: Vec<String> = (0..n_sessions).map(|_| uuid::Uuid::new_v4().to_string()).collect();
        for (i, id) in ids.iter().enumerate() {
            let dir = sessions.join(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("workspace.yaml"),
                format!("id: {id}\nname: \"Stress {i}\"\ncwd: C:\\\\stress\nclient_name: github/cli\ncreated_at: 2026-10-21T10:00:00Z\nupdated_at: 2026-10-21T10:00:00Z\n"),
            )
            .unwrap();
            let mut events = String::from("{\"type\":\"session.start\",\"data\":{}}\n{\"type\":\"user.message\",\"data\":{}}\n");
            for a in 0..n_agents {
                events.push_str(&format!("{{\"type\":\"subagent.started\",\"agentId\":\"pre{a}\",\"data\":{{\"toolCallId\":\"pre{a}\",\"agentName\":\"Agent {a}\"}}}}\n"));
                if a % 2 == 0 {
                    events.push_str(&format!("{{\"type\":\"subagent.completed\",\"agentId\":\"pre{a}\",\"data\":{{\"toolCallId\":\"pre{a}\"}}}}\n"));
                }
            }
            std::fs::write(dir.join("events.jsonl"), events).unwrap();
            std::fs::write(dir.join(format!("inuse.{pid}.lock")), "").unwrap();
            let (g, k) = (i / 10, i % 10);
            doc.nodes.push(Node {
                id: id.clone(),
                kind: NodeKind::Session,
                name: format!("Stress {i}"),
                x: g as i32 * 80 + 2 + (k as i32 % 2) * 36,
                y: (k as i32 / 2) * 10 + 2,
                cwd: Some(work.to_string_lossy().into_owned()),
                parent: None,
                agent_key: None,
                source: None,
                command: None,
                fresh: false,
                automation: None,
                synced_name: None, reviewed_ms: None, mcp_off: None, passive: None, no_messages: false,
            });
        }
        for g in 0..n_sessions.div_ceil(10) as i32 {
            doc.groups.push(Group { id: format!("g{g}"), name: format!("Group {g}"), x: g * 80, y: 0, w: 76, h: 54, color: (g % 6) as u8, folder: None, source: None, defaults: None });
        }
        let canvas = home.join("canvas.json");
        std::fs::write(&canvas, serde_json::to_string_pretty(&doc).unwrap()).unwrap();

        // 30 busy sessions append events continuously.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = {
            let (stop, sessions, busy) = (stop.clone(), sessions.clone(), ids[..30].to_vec());
            std::thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    n += 1;
                    for (i, id) in busy.iter().enumerate() {
                        let mut f = std::fs::OpenOptions::new().append(true).open(sessions.join(id).join("events.jsonl")).unwrap();
                        let mut lines = vec![
                            format!("{{\"type\":\"tool.execution_start\",\"data\":{{\"toolCallId\":\"t{n}\",\"toolName\":\"shell\"}}}}"),
                            format!("{{\"type\":\"tool.execution_complete\",\"data\":{{\"toolCallId\":\"t{n}\"}}}}"),
                        ];
                        if n.is_multiple_of(10) && n / 10 <= 12 {
                            lines.push(format!("{{\"type\":\"subagent.started\",\"agentId\":\"a{n}\",\"data\":{{\"toolCallId\":\"s{n}\",\"agentName\":\"Worker {n}\"}}}}"));
                        }
                        if n.is_multiple_of(15) {
                            lines.push(format!("{{\"type\":\"subagent.completed\",\"agentId\":\"a{}\",\"data\":{{\"toolCallId\":\"s{}\"}}}}", n - 5, n - 5));
                        }
                        if (n + i as u64).is_multiple_of(40) {
                            lines.push(format!("{{\"type\":\"permission.requested\",\"data\":{{\"requestId\":\"p{n}\"}}}}"));
                        }
                        if (n + i as u64) % 40 == 20 {
                            lines.push(format!("{{\"type\":\"permission.completed\",\"data\":{{\"requestId\":\"p{}\"}}}}", n - 20));
                        }
                        for line in lines {
                            let _ = writeln!(f, "{line}");
                        }
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            })
        };

        let instance = crate::paths::Instance::new(Some(format!("stress{pid}")), Some(home.clone()), Some(sessions.clone())).unwrap();
        let exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"target\release\copilot-canvas.exe");
        let args: Vec<String> = vec![
            "--perf".into(), "--instance".into(), instance.name.clone(), "--home".into(), home.to_string_lossy().into_owned(),
            "--sessions-dir".into(), sessions.to_string_lossy().into_owned(), "--cwd".into(), work.to_string_lossy().into_owned(),
        ];
        let mut ui = super::Console::start(&exe, &args, 320, 55).unwrap();
        let hud = |ui: &super::Console| ui.text().lines().last().unwrap_or_default().rsplit("  ").find(|s| s.contains("fps")).unwrap_or("").trim().to_owned();
        ui.wait("canvas", Duration::from_secs(30), |t| t.contains("Stress 1") && t.contains("fps")).unwrap();
        std::thread::sleep(Duration::from_secs(6));
        println!("IDLE (sessions busy, no input): {}", hud(&ui));
        let (x0, y0) = (100, 25);
        ui.send_now(format!("\x1b[<1;{x0};{y0}M").as_bytes()).unwrap();
        let started = Instant::now();
        let mut i = 0i32;
        while started.elapsed() < Duration::from_secs(5) {
            i += 1;
            ui.send_now(format!("\x1b[<33;{};{}M", x0 + ((i / 3) % 40) - 20, y0 + (i / 9) % 6).as_bytes()).unwrap();
            std::thread::sleep(Duration::from_millis(8));
        }
        ui.send_now(format!("\x1b[<1;{x0};{y0}m").as_bytes()).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        println!("PANNING (sessions busy): {}", hud(&ui));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = writer.join();
        ui.send(b"q").unwrap();
        std::thread::sleep(Duration::from_millis(500));
        ui.send(b"\x1b[C\r").unwrap();
        let _ = ui.wait_exit(Duration::from_secs(10));
        if let Ok(conn) = crate::client::conn::try_connect(&instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_secs(2));
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod user_message_canvas_probe {
    use std::time::Duration;

    /// Opens a copy of an old session from the canvas (sandboxed COPILOT_HOME)
    /// and checks that your messages are tinted. `PROBE_SESSION=<id>`.
    #[test]
    #[ignore]
    fn user_message_canvas_probe() {
        let id = std::env::var("PROBE_SESSION").unwrap();
        let root = std::env::temp_dir().join(format!("cc-umc-{}", uuid::Uuid::new_v4()));
        let (copilot_home, home, work) = (root.join("copilot"), root.join("canvas"), root.join("work"));
        let real = dirs::home_dir().unwrap().join(".copilot");
        let sessions = copilot_home.join("session-state");
        std::fs::create_dir_all(sessions.join(&id)).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::copy(real.join("config.json"), copilot_home.join("config.json")).unwrap();
        let _ = std::fs::copy(real.join("settings.json"), copilot_home.join("settings.json"));
        for f in ["workspace.yaml", "events.jsonl"] {
            std::fs::copy(real.join("session-state").join(&id).join(f), sessions.join(&id).join(f)).unwrap();
        }
        let mut doc = crate::client::doc::Doc::default();
        doc.nodes.push(crate::client::doc::Node {
            id: id.clone(), kind: crate::client::doc::NodeKind::Session, name: "Probe".into(), x: 2, y: 2,
            cwd: Some(work.to_string_lossy().into_owned()), parent: None, agent_key: None, source: None, command: None, fresh: false, automation: None, synced_name: None, reviewed_ms: None, mcp_off: None, passive: None, no_messages: false,
        });
        doc.selected = Some(id.clone());
        std::fs::write(home.join("canvas.json"), serde_json::to_string(&doc).unwrap()).unwrap();
        let instance = crate::paths::Instance::new(Some(format!("umc{}", std::process::id())), Some(home.clone()), Some(sessions.clone())).unwrap();
        let exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"target\release\copilot-canvas.exe");
        let args: Vec<String> = vec!["--instance".into(), instance.name.clone(), "--home".into(), home.to_string_lossy().into_owned(), "--sessions-dir".into(), sessions.to_string_lossy().into_owned(), "--cwd".into(), work.to_string_lossy().into_owned()];
        let mut ui = super::Console::start_with_env(&exe, &args, &[("COPILOT_HOME", copilot_home.as_path())], 200, 60).unwrap();
        ui.wait("canvas", Duration::from_secs(20), |t| t.contains("Probe")).unwrap();
        ui.send(b"\r").unwrap();
        let ok = ui.wait("resumed history", Duration::from_secs(90), |t| t.contains("❯ what is") || t.contains("❯ catch up"));
        std::thread::sleep(Duration::from_secs(4));
        let snap = canvas_host::emulator::Emulator::snapshot(&*crate::service::term::lock(&ui.screen));
        let user_bg = crate::protocol::COLOR_RGB | (30 << 16) | (44 << 8) | 70;
        let mut tinted = 0;
        for runs in &snap.lines {
            let text: String = runs.iter().map(|r| r.text.as_str()).collect();
            if runs.iter().any(|r| r.bg == user_bg) {
                tinted += 1;
                println!("TINTED: {}", text.trim().chars().take(100).collect::<String>());
            }
        }
        println!("resumed: {}, tinted rows: {tinted}", ok.is_ok());
        ui.send(b"\x1d").unwrap();
        ui.send(b"q").unwrap();
        let _ = ui.wait("quit", Duration::from_secs(5), |t| t.contains("Stop all and quit"));
        ui.send(b"\x1b[C\r").unwrap();
        let _ = ui.wait_exit(Duration::from_secs(20));
        if let Ok(conn) = crate::client::conn::try_connect(&instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_secs(3));
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod upgrade_probe {
    use std::time::Duration;

    use super::Console;

    /// The old build must speak the same protocol (otherwise the restart is required at startup).
    /// Manual (after `cargo build --release`): `UPGRADE_OLD_EXE=<older copilot-canvas.exe> cargo test --release upgrade_probe -- --ignored --nocapture`.
    /// An older build's service keeps a shell running; the new client keeps it at
    /// the startup prompt, then updates it from the toolbar and the shell resumes.
    #[test]
    #[ignore]
    fn upgrade_probe() {
        let old = std::path::PathBuf::from(std::env::var("UPGRADE_OLD_EXE").expect("UPGRADE_OLD_EXE"));
        let new = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), r"\target\release\copilot-canvas.exe"));
        let root = std::env::temp_dir().join(format!("cc-upgrade-{}", uuid::Uuid::new_v4()));
        let (home, sessions, work, copilot_home) = (root.join("home"), root.join("sessions"), root.join("work"), root.join("copilot-home"));
        for d in [&sessions, &work, &copilot_home] {
            std::fs::create_dir_all(d).unwrap();
        }
        let instance = crate::paths::Instance::new(Some(format!("upgrade{}", std::process::id())), Some(home.clone()), Some(sessions.clone())).unwrap();
        let args: Vec<String> = vec![
            "--demo".into(),
            "--instance".into(),
            instance.name.clone(),
            "--home".into(),
            home.to_string_lossy().into_owned(),
            "--sessions-dir".into(),
            sessions.to_string_lossy().into_owned(),
            "--cwd".into(),
            work.to_string_lossy().into_owned(),
        ];
        let env = [("COPILOT_HOME", copilot_home.as_path())];
        let result = (|| -> anyhow::Result<()> {
            println!("old build: a shell keeps running after the client detaches");
            let mut ui = Console::start_with_env(&old, &args, &env, 170, 46)?;
            ui.wait("old canvas", Duration::from_secs(20), |t| t.contains("Build session canvas"))?;
            ui.send(b"\r")?;
            ui.wait("shell", Duration::from_secs(20), |t| t.contains("PS "))?;
            ui.send(b"\x1d")?;
            ui.send(b"q")?;
            ui.wait("quit dialog", Duration::from_secs(5), |t| t.contains("Detach (keep running)"))?;
            ui.send(b"\r")?;
            ui.wait_exit(Duration::from_secs(10))?;

            println!("new build: keep the old service at the startup prompt");
            let mut ui = Console::start_with_env(&new, &args, &env, 170, 46)?;
            ui.wait("startup prompt", Duration::from_secs(20), |t| t.contains("Restart it now? [y/N]"))?;
            if std::env::var("UPGRADE_ANSWER").as_deref() == Ok("y") {
                // Restart at startup (required for an older protocol): the shell resumes.
                ui.send(b"y\r")?;
                ui.wait("resumed after the startup restart", Duration::from_secs(40), |t| {
                    t.contains("Terminal service updated; resumed 1 session(s)") && t.contains("Build session canvas")
                })?;
                anyhow::ensure!(!ui.text().contains("⟳ Update service"), "service is current");
                let (bx, by) = ui.locate("Build session canvas", super::CANVAS)?;
                ui.click(bx, by)?;
                ui.wait("terminal focus", Duration::from_secs(5), |t| t.contains("Typing goes to the terminal"))?;
                ui.wait("resumed shell", Duration::from_secs(20), |t| t.contains("PS "))?;
                ui.send(b"Write-Output ('UPGRADE_' + 'OK')\r")?;
                ui.wait("shell works", Duration::from_secs(10), |t| t.contains("UPGRADE_OK"))?;
                println!("startup restart: old service replaced, shell resumed");
                ui.send(b"\x1d")?;
                ui.send(b"q")?;
                ui.wait("quit dialog", Duration::from_secs(5), |t| t.contains("Stop all and quit"))?;
                ui.send(b"\x1b[C\r")?;
                ui.wait_exit(Duration::from_secs(10))?;
                return Ok(());
            }
            ui.send(b"n\r")?;
            ui.wait("update button", Duration::from_secs(20), |t| t.contains("⟳ Update service") && t.contains("Build session canvas"))?;

            println!("toolbar update restarts the service and resumes the shell");
            let (x, y) = ui.find("⟳ Update service").unwrap();
            ui.click(x + 2, y)?;
            ui.wait("confirm", Duration::from_secs(5), |t| t.contains("Update the terminal service?") && t.contains("1 running session(s)"))?;
            ui.send(b"\r")?;
            ui.wait("resumed", Duration::from_secs(40), |t| t.contains("Terminal service updated; resumed 1 session(s)"))?;
            anyhow::ensure!(!ui.text().contains("⟳ Update service"), "button gone after the update");
            let log = std::fs::read_to_string(instance.log_path()).unwrap_or_default();
            let builds: Vec<&str> = log.lines().filter_map(|l| l.split("service started, build ").nth(1)).collect();
            println!("service builds: {builds:?}");
            // The button is gone, so the new service matches the client build.
            anyhow::ensure!(builds.len() == 2 && builds[0] != builds[1], "the service was replaced by another build");
            let (bx, by) = ui.locate("Build session canvas", super::CANVAS)?;
            ui.click(bx, by)?;
            ui.wait("terminal focus", Duration::from_secs(5), |t| t.contains("Typing goes to the terminal"))?;
            ui.wait("resumed shell", Duration::from_secs(20), |t| t.contains("PS "))?;
            ui.send(b"Write-Output ('UPGRADE_' + 'OK')\r")?;
            ui.wait("shell works", Duration::from_secs(10), |t| t.contains("UPGRADE_OK"))?;
            ui.send(b"\x1d")?;
            ui.send(b"q")?;
            ui.wait("quit dialog", Duration::from_secs(5), |t| t.contains("Stop all and quit"))?;
            ui.send(b"\x1b[C\r")?;
            ui.wait_exit(Duration::from_secs(10))?;
            Ok(())
        })();
        if let Ok(conn) = crate::client::conn::try_connect(&instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_millis(2500));
        }
        result.unwrap();
        println!("PASS");
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod image_paste_probe {
    use std::time::Duration;

    /// Manual: put an image on the clipboard, then
    /// `cargo test --release image_paste_probe -- --ignored --nocapture`.
    /// Runs real Copilot (sandboxed COPILOT_HOME, no prompt sent) and shows how
    /// it reacts to each paste keystroke.
    #[test]
    #[ignore]
    fn image_paste_probe() {
        let root = std::env::temp_dir().join(format!("cc-img-{}", uuid::Uuid::new_v4()));
        let (copilot_home, work) = (root.join("copilot"), root.join("work"));
        let real = dirs::home_dir().unwrap().join(".copilot");
        std::fs::create_dir_all(&copilot_home).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::copy(real.join("config.json"), copilot_home.join("config.json")).unwrap();
        let _ = std::fs::copy(real.join("settings.json"), copilot_home.join("settings.json"));
        let exe = crate::copilot_cfg::copilot_exe().unwrap();
        let keys: Vec<(&str, Vec<u8>)> = std::env::var("PASTE_KEYS").ok().map(|k| vec![("custom", k.replace("ESC", "\x1b").into_bytes())]).unwrap_or_else(|| vec![
            ("legacy Ctrl+V", b"\x16".to_vec()),
            ("kitty Ctrl+V", b"\x1b[118;5u".to_vec()),
            ("Alt+V", b"\x1bv".to_vec()),
        ]);
        let kitty = std::env::var_os("NO_KITTY").is_none();
        let args: Vec<String> = vec!["--allow-all".into(), "--disallow-temp-dir".into()];
        let mut env: Vec<(&str, &std::path::Path)> = vec![("COPILOT_HOME", copilot_home.as_path())];
        let off = std::path::PathBuf::from("false");
        env.push(("COPILOT_AUTO_UPDATE", off.as_path()));
        std::env::set_current_dir(&work).unwrap();
        let mut ui = super::Console::start_full(&exe, &args, &env, 140, 40, kitty).unwrap();
        let ready = ui.wait("copilot input", Duration::from_secs(60), |t| t.contains("Type @ to mention") || t.contains("? for shortcuts") || t.contains("trust") || t.contains("❯"));
        std::thread::sleep(Duration::from_secs(3));
        if ui.text().to_lowercase().contains("trust") {
            println!("--- trust screen ---\n{}", ui.text());
            ui.send(b"\r").unwrap();
            std::thread::sleep(Duration::from_secs(4));
        }
        println!("ready: {} kitty: {kitty} mode: {}", ready.is_ok(), ui.mode());
        for (name, bytes) in keys {
            ui.send(&bytes).unwrap();
            std::thread::sleep(Duration::from_secs(3));
            let text = ui.text();
            let tail: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
            println!("=== after {name} ===\n{}", tail[tail.len().saturating_sub(8)..].join("\n"));
        }
        let _ = ui.send(b"\x03");
        std::thread::sleep(Duration::from_millis(500));
        let _ = ui.send(b"\x03");
        let _ = ui.wait_exit(Duration::from_secs(10));
        std::env::set_current_dir(std::env::temp_dir()).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod passthrough_probe {
    use std::time::Duration;

    /// Manual: `CC_RAW_CAPTURE=<file> cargo test --release passthrough_probe -- --ignored --nocapture`.
    /// Does ConPTY pass OSC 9;4 / OSC 777 / BEL through to the terminal?
    #[test]
    #[ignore]
    fn passthrough_probe() {
        let script = "$e=[char]27; $b=[char]7; Write-Host -NoNewline \"$e]9;4;3;0$b\"; Write-Host -NoNewline \"$e]777;notify;Title;Body$b\"; Write-Host -NoNewline \"$b\"; Write-Host -NoNewline \"$e]0;MyTitle$b\"; Write-Host 'DONE_MARK'; Start-Sleep 2";
        let args: Vec<String> = vec!["-NoProfile".into(), "-Command".into(), script.into()];
        let mut ui = super::Console::start(std::path::Path::new("powershell.exe"), &args, 100, 20).unwrap();
        ui.wait("done", Duration::from_secs(20), |t| t.contains("DONE_MARK")).unwrap();
        let _ = ui.wait_exit(Duration::from_secs(10));
    }
}

#[cfg(test)]
mod canvas_image_paste_probe {
    use std::time::Duration;

    use anyhow::Context;

    /// Manual: image on the clipboard, then
    /// `cargo test --release canvas_image_paste_probe -- --ignored --nocapture`.
    /// A new Copilot session in a sandboxed COPILOT_HOME (no prompt is sent).
    #[test]
    #[ignore]
    fn canvas_image_paste_probe() {
        let root = std::env::temp_dir().join(format!("cc-cip-{}", uuid::Uuid::new_v4()));
        let (copilot_home, home, work) = (root.join("copilot"), root.join("canvas"), root.join("work"));
        let real = dirs::home_dir().unwrap().join(".copilot");
        let sessions = copilot_home.join("session-state");
        for d in [&sessions, &home, &work] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::copy(real.join("config.json"), copilot_home.join("config.json")).unwrap();
        let _ = std::fs::copy(real.join("settings.json"), copilot_home.join("settings.json"));
        if std::env::var_os("PROBE_MOUSE").is_some() {
            let path = copilot_home.join("settings.json");
            let mut settings: serde_json::Value = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(serde_json::json!({}));
            settings["mouse"] = serde_json::json!(true);
            std::fs::write(&path, settings.to_string()).unwrap();
        }
        let id = uuid::Uuid::new_v4().to_string();
        let mut doc = crate::client::doc::Doc::default();
        doc.nodes.push(crate::client::doc::Node {
            id: id.clone(), kind: crate::client::doc::NodeKind::Session, name: "Paste probe".into(), x: 2, y: 2,
            cwd: Some(work.to_string_lossy().into_owned()), parent: None, agent_key: None, source: None, command: None, fresh: true, automation: None, synced_name: None, reviewed_ms: None, mcp_off: None, passive: None, no_messages: false,
        });
        doc.selected = Some(id.clone());
        std::fs::write(home.join("canvas.json"), serde_json::to_string(&doc).unwrap()).unwrap();
        let instance = crate::paths::Instance::new(Some(format!("cip{}", std::process::id())), Some(home.clone()), Some(sessions.clone())).unwrap();
        let exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"target\release\copilot-canvas.exe");
        let args: Vec<String> = vec!["--instance".into(), instance.name.clone(), "--home".into(), home.to_string_lossy().into_owned(), "--sessions-dir".into(), sessions.to_string_lossy().into_owned(), "--cwd".into(), work.to_string_lossy().into_owned()];
        let mut ui = super::Console::start_with_env(&exe, &args, &[("COPILOT_HOME", copilot_home.as_path())], 200, 50).unwrap();
        let count = |t: &str| t.matches("📷").count();
        let result = (|| -> anyhow::Result<()> {
            ui.wait("canvas", Duration::from_secs(20), |t| t.contains("Paste probe"))?;
            ui.send(b"\r")?;
            ui.wait("copilot input", Duration::from_secs(90), |t| t.contains("/ commands"))?;
            std::thread::sleep(Duration::from_secs(3));
            // The test image goes on the clipboard only now (CLIP_SET script saves yours first).
            if let Some(script) = std::env::var_os("CLIP_SET") {
                let _ = std::process::Command::new("powershell").args(["-STA", "-NoProfile", "-File"]).arg(script).status();
            }
            ui.send(b"\x1bv")?;
            ui.wait("Alt+V attaches the image", Duration::from_secs(10), |t| count(t) >= 1)?;
            println!("Alt+V: attached");
            ui.send(b"\x16")?;
            ui.wait("Ctrl+V attaches the image", Duration::from_secs(10), |t| count(t) >= 2)?;
            println!("Ctrl+V: attached");
            let (x, y) = ui.find("❯ [").context("input line")?;
            ui.send(format!("\x1b[<2;{};{}M\x1b[<2;{};{}m", x + 1, y + 1, x + 1, y + 1).as_bytes())?;
            ui.wait("pane menu", Duration::from_secs(5), |t| t.contains("Paste"))?;
            // The menu row (not the session named "Paste probe" nor the status hint).
            let text = ui.text();
            let (py, line) = text.lines().enumerate().find(|(_, l)| l.contains(" Paste ") && l.contains("Ctrl+V") && !l.contains("Ctrl+V paste")).context("Paste item")?;
            let px = crate::client::paint::text_width(&line[..line.find(" Paste ").unwrap_or(0)]) as u16 + 1;
            let py = py as u16;
            ui.click(px, py)?;
            ui.wait("menu Paste attaches the image", Duration::from_secs(10), |t| count(t) >= 3)?;
            println!("menu Paste: attached");
            if std::env::var_os("PROBE_MOUSE").is_some() {
                let (tx, ty) = ui.find(" Sessions ").context("Copilot tab bar")?;
                ui.click(tx + 2, ty)?;
                ui.wait("click switched Copilot's tab", Duration::from_secs(10), |t| t.contains("[Sessions]"))?;
                println!("click: reached Copilot (tab switched)");
            }
            Ok(())
        })();
        if let Some(script) = std::env::var_os("CLIP_RESTORE") {
            let _ = std::process::Command::new("powershell").args(["-STA", "-NoProfile", "-File"]).arg(script).status();
        }
        if result.is_err() {
            let text = ui.text();
            let lines: Vec<&str> = text.lines().collect();
            println!("--- screen (bottom) ---\n{}", lines[lines.len().saturating_sub(14)..].join("\n"));
        }
        ui.send(b"\x15").ok();
        ui.send(b"\x1d").ok();
        ui.send(b"q").ok();
        let _ = ui.wait("quit", Duration::from_secs(5), |t| t.contains("Stop all and quit"));
        ui.send(b"\x1b[C\r").ok();
        let _ = ui.wait_exit(Duration::from_secs(20));
        if let Ok(conn) = crate::client::conn::try_connect(&instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_secs(3));
        }
        let _ = std::fs::remove_dir_all(&root);
        result.unwrap();
    }
}

#[cfg(test)]
mod fork_probe {
    use std::time::Duration;

    fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() { out.extend(walk(&p)); } else { out.push(p); }
        }
        out
    }

    fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap().flatten() {
            let (src, dst) = (e.path(), to.join(e.file_name()));
            if src.is_dir() { copy_tree(&src, &dst); } else if !e.file_name().to_string_lossy().starts_with("inuse.") { std::fs::copy(&src, &dst).unwrap(); }
        }
    }

    /// Manual: `PROBE_SESSION=<id> cargo test --release fork_probe -- --ignored --nocapture`.
    /// Runs `/fork` on a sandboxed copy of a session and prints what Copilot wrote.
    #[test]
    #[ignore]
    fn fork_probe() {
        let id = std::env::var("PROBE_SESSION").unwrap();
        let root = std::env::temp_dir().join(format!("cc-fork-{}", uuid::Uuid::new_v4()));
        let (copilot_home, work) = (root.join("copilot"), root.join("work"));
        let real = dirs::home_dir().unwrap().join(".copilot");
        let sessions = copilot_home.join("session-state");
        std::fs::create_dir_all(sessions.join(&id)).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::copy(real.join("config.json"), copilot_home.join("config.json")).unwrap();
        let _ = std::fs::copy(real.join("settings.json"), copilot_home.join("settings.json"));
        copy_tree(&real.join("session-state").join(&id), &sessions.join(&id));
        let exe = crate::copilot_cfg::copilot_exe().unwrap();
        let args: Vec<String> = vec!["--resume".into(), id.clone(), "--disable-builtin-mcps".into()];
        let off = std::path::PathBuf::from("false");
        let env: Vec<(&str, &std::path::Path)> = vec![("COPILOT_HOME", copilot_home.as_path()), ("COPILOT_AUTO_UPDATE", off.as_path())];
        std::env::set_current_dir(&work).unwrap();
        let mut ui = super::Console::start_full(&exe, &args, &env, 140, 40, true).unwrap();
        let _ = ui.wait("ready", Duration::from_secs(90), |t| t.contains("/ commands") || t.contains("❯"));
        std::thread::sleep(Duration::from_secs(4));
        ui.send(b"/fork Probe fork").unwrap();
        std::thread::sleep(Duration::from_millis(500));
        ui.send(b"\r").unwrap();
        std::thread::sleep(Duration::from_secs(8));
        let text = ui.text();
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        println!("--- screen tail ---\n{}", lines[lines.len().saturating_sub(10)..].join("\n"));
        for entry in std::fs::read_dir(&sessions).unwrap().flatten() {
            let dir = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            println!("=== {name}{}", if name == id { " (original)" } else { " (NEW)" });
            let mut files: Vec<String> = walk(&dir).into_iter().map(|p| format!("{} ({} B)", p.strip_prefix(&dir).unwrap().display(), std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0))).collect();
            files.sort();
            println!("files: {files:?}");
            println!("{}", std::fs::read_to_string(dir.join("workspace.yaml")).unwrap_or_default());
            let events = std::fs::read_to_string(dir.join("events.jsonl")).unwrap_or_default();
            let kinds: Vec<String> = events.lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()).map(|v| v["type"].as_str().unwrap_or("?").to_owned()).collect();
            println!("events: {} lines; first: {:?}; last: {:?}", kinds.len(), kinds.iter().take(4).collect::<Vec<_>>(), kinds.iter().rev().take(4).collect::<Vec<_>>());
            if name != id {
                for line in events.lines().take(2) {
                    println!("  {}", &line[..line.len().min(900)]);
                }
            }
        }
        let _ = ui.send(b"\x03");
        std::thread::sleep(Duration::from_millis(500));
        let _ = ui.send(b"\x03");
        let _ = ui.wait_exit(Duration::from_secs(10));
        std::env::set_current_dir(std::env::temp_dir()).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod fork_canvas_probe {
    use std::time::Duration;

    use anyhow::Context;

    /// Manual: `PROBE_SESSION=<id> PROBE_TEXT=<text from its first message> cargo test --release fork_canvas_probe -- --ignored --nocapture`.
    /// New subsession of a sandboxed copy of a session: the child must open with the parent's conversation.
    #[test]
    #[ignore]
    fn fork_canvas_probe() {
        let id = std::env::var("PROBE_SESSION").unwrap();
        let needle = std::env::var("PROBE_TEXT").unwrap();
        let root = std::env::temp_dir().join(format!("cc-fkc-{}", uuid::Uuid::new_v4()));
        let (copilot_home, home, work) = (root.join("copilot"), root.join("canvas"), root.join("work"));
        let real = dirs::home_dir().unwrap().join(".copilot");
        let sessions = copilot_home.join("session-state");
        for d in [&sessions.join(&id), &home, &work] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::copy(real.join("config.json"), copilot_home.join("config.json")).unwrap();
        let _ = std::fs::copy(real.join("settings.json"), copilot_home.join("settings.json"));
        for f in ["workspace.yaml", "events.jsonl"] {
            std::fs::copy(real.join("session-state").join(&id).join(f), sessions.join(&id).join(f)).unwrap();
        }
        let parent_events = std::fs::read(sessions.join(&id).join("events.jsonl")).unwrap();
        let mut doc = crate::client::doc::Doc::default();
        doc.nodes.push(crate::client::doc::Node {
            id: id.clone(), kind: crate::client::doc::NodeKind::Session, name: "Fork parent".into(), x: 2, y: 2,
            cwd: Some(work.to_string_lossy().into_owned()), parent: None, agent_key: None, source: None, command: None, fresh: false, automation: None, synced_name: None, reviewed_ms: None, mcp_off: None, passive: None, no_messages: false,
        });
        doc.selected = Some(id.clone());
        std::fs::write(home.join("canvas.json"), serde_json::to_string(&doc).unwrap()).unwrap();
        let instance = crate::paths::Instance::new(Some(format!("fkc{}", std::process::id())), Some(home.clone()), Some(sessions.clone())).unwrap();
        let exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"target\release\copilot-canvas.exe");
        let args: Vec<String> = vec!["--instance".into(), instance.name.clone(), "--home".into(), home.to_string_lossy().into_owned(), "--sessions-dir".into(), sessions.to_string_lossy().into_owned(), "--cwd".into(), work.to_string_lossy().into_owned()];
        let mut ui = super::Console::start_with_env(&exe, &args, &[("COPILOT_HOME", copilot_home.as_path())], 200, 50).unwrap();
        let result = (|| -> anyhow::Result<()> {
            ui.wait("canvas", Duration::from_secs(20), |t| t.contains("Fork parent"))?;
            ui.send(b"s")?;
            ui.wait("subsession dialog with fork default", Duration::from_secs(10), |t| t.contains("New subsession") && t.contains("◉ Fork parent's conversation"))?;
            ui.send(b"Fork probe child\r")?;
            ui.wait("child opens with the parent's conversation", Duration::from_secs(120), |t| t.contains("Fork probe child · ") && t.contains(&needle))?;
            let doc: crate::client::doc::Doc = serde_json::from_str(&std::fs::read_to_string(home.join("canvas.json"))?)?;
            let child = doc.nodes.iter().find(|n| n.name == "Fork probe child").context("child node")?;
            anyhow::ensure!(child.parent.as_deref() == Some(id.as_str()), "linked to its parent");
            let yaml = std::fs::read_to_string(sessions.join(&child.id).join("workspace.yaml")).context("forked session on disk")?;
            anyhow::ensure!(yaml.contains("Fork probe child"), "fork named: {yaml}");
            anyhow::ensure!(std::fs::read(sessions.join(&id).join("events.jsonl"))? == parent_events, "parent history untouched");
            println!("forked child {} opened with the parent's conversation", &child.id[..8]);
            Ok(())
        })();
        if result.is_err() {
            let text = ui.text();
            let lines: Vec<&str> = text.lines().collect();
            println!("--- screen (bottom) ---\n{}", lines[lines.len().saturating_sub(25)..].join("\n"));
        }
        ui.send(b"\x1d").ok();
        ui.send(b"q").ok();
        let _ = ui.wait("quit", Duration::from_secs(5), |t| t.contains("Stop all and quit"));
        ui.send(b"\x1b[C\r").ok();
        let _ = ui.wait_exit(Duration::from_secs(20));
        if let Ok(conn) = crate::client::conn::try_connect(&instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_secs(3));
        }
        let _ = std::fs::remove_dir_all(&root);
        result.unwrap();
    }
}

#[cfg(test)]
mod mcp_picker_probe {
    use std::time::Duration;

    use anyhow::Context;

    /// Manual: `cargo test --release mcp_picker_probe -- --ignored --nocapture` (sandboxed COPILOT_HOME, no prompt sent).
    #[test]
    #[ignore]
    fn mcp_picker_probe() {
        let root = std::env::temp_dir().join(format!("cc-mcp-{}", uuid::Uuid::new_v4()));
        let (copilot_home, home, work) = (root.join("copilot"), root.join("canvas"), root.join("work"));
        let real = dirs::home_dir().unwrap().join(".copilot");
        let sessions = copilot_home.join("session-state");
        for d in [&sessions, &home, &work] {
            std::fs::create_dir_all(d).unwrap();
        }
        for f in ["config.json", "settings.json", "mcp-config.json"] {
            let _ = std::fs::copy(real.join(f), copilot_home.join(f));
        }
        let instance = crate::paths::Instance::new(Some(format!("mcp{}", std::process::id())), Some(home.clone()), Some(sessions.clone())).unwrap();
        let exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"target\release\copilot-canvas.exe");
        let args: Vec<String> = vec!["--instance".into(), instance.name.clone(), "--home".into(), home.to_string_lossy().into_owned(), "--sessions-dir".into(), sessions.to_string_lossy().into_owned(), "--cwd".into(), work.to_string_lossy().into_owned()];
        std::fs::write(home.join("canvas.json"), serde_json::to_string(&crate::client::doc::Doc::default()).unwrap()).unwrap();
        let mut ui = super::Console::start_with_env(&exe, &args, &[("COPILOT_HOME", copilot_home.as_path())], 200, 50).unwrap();
        let result = (|| -> anyhow::Result<()> {
            ui.wait("canvas", Duration::from_secs(20), |t| t.contains("Your canvas is empty"))?;
            ui.send(b"n")?;
            ui.wait("dialog with MCP row", Duration::from_secs(60), |t| t.contains("MCP servers") && t.contains(" on") && t.contains("▾ choose"))?;
            ui.send(b"MCP probe")?;
            for _ in 0..4 {
                ui.send(b"\t")?;
            }
            ui.send(b"\r")?;
            ui.wait("picker", Duration::from_secs(10), |t| t.contains("MCP servers for the new session") && t.contains("[x]"))?;
            ui.send(b"n")?;
            ui.wait("none selected", Duration::from_secs(5), |t| t.contains(" 0 of ") && !t.contains("[x]"))?;
            ui.send(b"\x13")?;
            ui.wait("preset prompt", Duration::from_secs(5), |t| t.contains("Save MCP preset"))?;
            ui.send(b"Minimal\r")?;
            ui.wait("preset saved and chosen", Duration::from_secs(5), |t| t.contains(" Minimal ") && t.contains("MCP servers for the new session"))?;
            ui.send(b"\r")?;
            ui.wait("dialog shows the choice", Duration::from_secs(5), |t| t.contains("0 of ") && t.contains("“None”"))?;
            ui.send(b"\t\t\r")?;
            ui.wait("session started", Duration::from_secs(90), |t| t.contains("MCP probe · ") && t.contains("❯"))?;
            std::thread::sleep(Duration::from_secs(20));
            let log = std::fs::read_to_string(instance.log_path())?;
            let pid = log.lines().rev().find_map(|l| l.split(" pid ").nth(1)).and_then(|p| p.trim().parse::<u32>().ok()).context("pid")?;
            let query = format!("$c=(Get-CimInstance Win32_Process -Filter 'ProcessId={pid}').CommandLine; ([regex]::Matches($c,'--disable-mcp-server')).Count; $all=Get-CimInstance Win32_Process; $kids=@{{}}; foreach($p in $all){{ $kids[[int]$p.ParentProcessId]+=@($p) }}; $q=@({pid}); $n=0; $agency=0; while($q.Count){{ $c=$q[0]; $q=@($q | Select-Object -Skip 1); foreach($k in @($kids[[int]$c])){{ if($k){{ $n++; if($k.Name -eq 'agency.exe'){{$agency++}}; $q+=[int]$k.ProcessId }} }} }}; $n; $agency");
            let out = std::process::Command::new("powershell.exe").args(["-NoProfile", "-Command", &query]).output()?;
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            let nums: Vec<u32> = text.lines().filter_map(|l| l.trim().parse().ok()).collect();
            println!("--disable-mcp-server flags: {} · processes under the session: {} · agency.exe: {}", nums[0], nums[1], nums[2]);
            anyhow::ensure!(nums[0] > 0, "flags passed");
            ui.send(b"\x1d")?;
            ui.send(b"q")?;
            ui.wait("quit", Duration::from_secs(5), |t| t.contains("Stop all and quit"))?;
            ui.send(b"\x1b[C\r")?;
            let _ = ui.wait_exit(Duration::from_secs(20));
            let doc: crate::client::doc::Doc = serde_json::from_str(&std::fs::read_to_string(home.join("canvas.json"))?)?;
            anyhow::ensure!(doc.mcp.presets.iter().any(|p| p.name == "Minimal"), "preset remembered");
            anyhow::ensure!(!doc.mcp.last_off.is_empty(), "last choice remembered");
            anyhow::ensure!(doc.nodes.iter().any(|n| n.name == "MCP probe" && n.mcp_off.as_ref().is_some_and(|o| !o.is_empty())), "session remembers its servers");
            println!("remembered: preset Minimal, last choice ({} off), per-session list", doc.mcp.last_off.len());
            Ok(())
        })();
        if result.is_err() {
            let text = ui.text();
            let lines: Vec<&str> = text.lines().collect();
            println!("--- screen (bottom) ---\n{}", lines[lines.len().saturating_sub(30)..].join("\n"));
            ui.send(b"\x1b").ok();
            ui.send(b"\x1d").ok();
            ui.send(b"q").ok();
            ui.send(b"\x1b[C\r").ok();
            let _ = ui.wait_exit(Duration::from_secs(20));
        }
        if let Ok(conn) = crate::client::conn::try_connect(&instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_secs(3));
        }
        let _ = std::fs::remove_dir_all(&root);
        result.unwrap();
    }
}

#[cfg(test)]
mod progress_probe {
    use std::time::Duration;

    /// Manual: `CC_RAW_CAPTURE=<file> cargo test --release progress_probe -- --ignored --nocapture`.
    /// One trivial turn in a sandboxed session (MCP servers off) to record Copilot's OSC 9;4 sequence.
    #[test]
    #[ignore]
    fn progress_probe() {
        let root = std::env::temp_dir().join(format!("cc-prog-{}", uuid::Uuid::new_v4()));
        let (copilot_home, work) = (root.join("copilot"), root.join("work"));
        let real = dirs::home_dir().unwrap().join(".copilot");
        std::fs::create_dir_all(&copilot_home).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::copy(real.join("config.json"), copilot_home.join("config.json")).unwrap();
        let _ = std::fs::copy(real.join("settings.json"), copilot_home.join("settings.json"));
        let exe = crate::copilot_cfg::copilot_exe().unwrap();
        let mut args: Vec<String> = vec!["--allow-all".into(), "--disable-builtin-mcps".into(), "-i".into(), "Reply with just the word OK and nothing else.".into()];
        if let Some(c) = crate::copilot_cfg::snapshot().mcp.into_iter().map(|s| s.name).collect::<Vec<_>>().into_iter().reduce(|a, b| format!("{a}\n{b}")) {
            for name in c.lines() {
                args.extend(["--disable-mcp-server".into(), name.to_owned()]);
            }
        }
        let off = std::path::PathBuf::from("false");
        let env: Vec<(&str, &std::path::Path)> = vec![("COPILOT_HOME", copilot_home.as_path()), ("COPILOT_AUTO_UPDATE", off.as_path())];
        std::env::set_current_dir(&work).unwrap();
        let mut ui = super::Console::start_full(&exe, &args, &env, 140, 40, true).unwrap();
        let _ = ui.wait("reply", Duration::from_secs(120), |t| t.lines().any(|l| l.trim() == "● OK" || l.trim() == "OK"));
        std::thread::sleep(Duration::from_secs(8));
        let text = ui.text();
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        println!("--- screen tail ---\n{}", lines[lines.len().saturating_sub(8)..].join("\n"));
        let events = std::fs::read_dir(copilot_home.join("session-state")).unwrap().flatten().next().map(|d| std::fs::read_to_string(d.path().join("events.jsonl")).unwrap_or_default()).unwrap_or_default();
        let kinds: Vec<String> = events.lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()).map(|v| v["type"].as_str().unwrap_or("?").to_owned()).collect();
        println!("events: {}", kinds.join(" > "));
        let _ = ui.send(b"\x03");
        std::thread::sleep(Duration::from_millis(500));
        let _ = ui.send(b"\x03");
        let _ = ui.wait_exit(Duration::from_secs(10));
        std::env::set_current_dir(std::env::temp_dir()).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod status_canvas_probe {
    use std::time::{Duration, Instant};

    /// Manual: `cargo test --release status_canvas_probe -- --ignored --nocapture`.
    /// One trivial turn in a sandboxed session started from the canvas (MCP servers off):
    /// records when the box shows working and when it shows done · review.
    #[test]
    #[ignore]
    fn status_canvas_probe() {
        let root = std::env::temp_dir().join(format!("cc-stat-{}", uuid::Uuid::new_v4()));
        let (copilot_home, home, work) = (root.join("copilot"), root.join("canvas"), root.join("work"));
        let real = dirs::home_dir().unwrap().join(".copilot");
        let sessions = copilot_home.join("session-state");
        for d in [&sessions, &home, &work] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::copy(real.join("config.json"), copilot_home.join("config.json")).unwrap();
        let _ = std::fs::copy(real.join("settings.json"), copilot_home.join("settings.json"));
        let mut doc = crate::client::doc::Doc::default();
        doc.mcp.last_off = crate::copilot_cfg::snapshot().mcp.into_iter().map(|s| s.name).collect();
        std::fs::write(home.join("canvas.json"), serde_json::to_string(&doc).unwrap()).unwrap();
        let instance = crate::paths::Instance::new(Some(format!("stat{}", std::process::id())), Some(home.clone()), Some(sessions.clone())).unwrap();
        let exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"target\release\copilot-canvas.exe");
        let args: Vec<String> = vec!["--instance".into(), instance.name.clone(), "--home".into(), home.to_string_lossy().into_owned(), "--sessions-dir".into(), sessions.to_string_lossy().into_owned(), "--cwd".into(), work.to_string_lossy().into_owned()];
        let mut ui = super::Console::start_with_env(&exe, &args, &[("COPILOT_HOME", copilot_home.as_path())], 200, 50).unwrap();
        let result = (|| -> anyhow::Result<()> {
            ui.wait("canvas", Duration::from_secs(20), |t| t.contains("Your canvas is empty"))?;
            ui.send(b"n")?;
            ui.wait("dialog", Duration::from_secs(30), |t| t.contains("First prompt") && t.contains("▾ choose"))?;
            ui.send(b"Status probe")?;
            for _ in 0..5 {
                ui.send(b"\t")?;
            }
            ui.send(b"Reply with just the word OK and nothing else.\r")?;
            std::thread::sleep(Duration::from_millis(800));
            ui.send(b"\x1d")?; // back to the canvas, so the finished turn waits for review
            let start = Instant::now();
            let header = |t: &str| t.lines().find(|l| l.contains("Status probe · ")).map(|l| l.to_owned()).unwrap_or_default();
            let (mut working, mut replied, mut done) = (None, None, None);
            while start.elapsed() < Duration::from_secs(150) && done.is_none() {
                let text = ui.text();
                let h = header(&text);
                if working.is_none() && h.contains("· working") {
                    working = Some(start.elapsed());
                }
                if replied.is_none() && text.lines().any(|l| l.contains("● OK")) {
                    replied = Some(start.elapsed());
                }
                if replied.is_some() && h.contains("done · review") {
                    done = Some(start.elapsed());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            println!("working seen at {working:?}, reply visible at {replied:?}, done · review at {done:?}");
            let (Some(r), Some(d)) = (replied, done) else { anyhow::bail!("did not reach done · review") };
            println!("reply → done · review: {} ms", d.saturating_sub(r).as_millis());
            anyhow::ensure!(working.is_some(), "working was shown");
            Ok(())
        })();
        if result.is_err() {
            let text = ui.text();
            let lines: Vec<&str> = text.lines().collect();
            println!("--- screen (bottom) ---\n{}", lines[lines.len().saturating_sub(20)..].join("\n"));
        }
        ui.send(b"q").ok();
        let _ = ui.wait("quit", Duration::from_secs(5), |t| t.contains("Stop all and quit"));
        ui.send(b"\x1b[C\r").ok();
        let _ = ui.wait_exit(Duration::from_secs(20));
        if let Ok(conn) = crate::client::conn::try_connect(&instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_secs(3));
        }
        let _ = std::fs::remove_dir_all(&root);
        result.unwrap();
    }
}

#[cfg(test)]
mod loop_canvas_probe {
    use std::io::Write;
    use std::time::Duration;

    /// Manual: `PROBE_SESSION=<id> cargo test --release loop_canvas_probe -- --ignored --nocapture`.
    /// A resumed sandbox copy of a session gets Copilot's schedule events appended:
    /// the box must loop while the schedule is active and stop when it is cancelled.
    #[test]
    #[ignore]
    fn loop_canvas_probe() {
        let id = std::env::var("PROBE_SESSION").unwrap();
        let root = std::env::temp_dir().join(format!("cc-loop-{}", uuid::Uuid::new_v4()));
        let (copilot_home, home, work) = (root.join("copilot"), root.join("canvas"), root.join("work"));
        let real = dirs::home_dir().unwrap().join(".copilot");
        let sessions = copilot_home.join("session-state");
        for d in [&sessions.join(&id), &home, &work] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::copy(real.join("config.json"), copilot_home.join("config.json")).unwrap();
        let _ = std::fs::copy(real.join("settings.json"), copilot_home.join("settings.json"));
        for f in ["workspace.yaml", "events.jsonl"] {
            std::fs::copy(real.join("session-state").join(&id).join(f), sessions.join(&id).join(f)).unwrap();
        }
        let mut doc = crate::client::doc::Doc::default();
        doc.nodes.push(crate::client::doc::Node {
            id: id.clone(), kind: crate::client::doc::NodeKind::Session, name: "Loop probe".into(), x: 2, y: 2,
            cwd: Some(work.to_string_lossy().into_owned()), parent: None, agent_key: None, source: None, command: None, fresh: false, automation: None, synced_name: None, reviewed_ms: None,
            mcp_off: Some(crate::copilot_cfg::snapshot().mcp.into_iter().map(|s| s.name).collect()),
            passive: None,
            no_messages: false,
        });
        doc.selected = Some(id.clone());
        std::fs::write(home.join("canvas.json"), serde_json::to_string(&doc).unwrap()).unwrap();
        let instance = crate::paths::Instance::new(Some(format!("loop{}", std::process::id())), Some(home.clone()), Some(sessions.clone())).unwrap();
        let exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"target\release\copilot-canvas.exe");
        let args: Vec<String> = vec!["--instance".into(), instance.name.clone(), "--home".into(), home.to_string_lossy().into_owned(), "--sessions-dir".into(), sessions.to_string_lossy().into_owned(), "--cwd".into(), work.to_string_lossy().into_owned()];
        let mut ui = super::Console::start_with_env(&exe, &args, &[("COPILOT_HOME", copilot_home.as_path())], 200, 50).unwrap();
        let events = sessions.join(&id).join("events.jsonl");
        let append = |line: String| {
            let mut f = std::fs::OpenOptions::new().append(true).open(&events).unwrap();
            writeln!(f, "{line}").unwrap();
        };
        let result = (|| -> anyhow::Result<()> {
            ui.wait("canvas", Duration::from_secs(20), |t| t.contains("Loop probe"))?;
            ui.send(b"\r")?;
            ui.wait("copilot resumed", Duration::from_secs(90), |t| t.contains("/ commands"))?;
            std::thread::sleep(Duration::from_secs(2));
            ui.send(b"\x1d")?;
            anyhow::ensure!(!ui.text().contains("↻"), "no loop before a schedule exists");
            let now = crate::paths::rfc3339_now();
            append(format!(r#"{{"type":"session.schedule_created","data":{{"id":7,"intervalMs":900000,"prompt":"probe","recurring":true,"origin":"model"}},"id":"probe-1","timestamp":"{now}","parentId":null}}"#));
            ui.wait("loop shown", Duration::from_secs(10), |t| t.contains("↻ every 15m") && t.contains("◂") && t.contains("↻ Loop probe"))?;
            println!("schedule created → box loops (↻ every 15m)");
            let now = crate::paths::rfc3339_now();
            append(format!(r#"{{"type":"session.schedule_cancelled","data":{{"id":7}},"id":"probe-2","timestamp":"{now}","parentId":null}}"#));
            ui.wait("loop removed", Duration::from_secs(10), |t| !t.contains("↻"))?;
            println!("schedule cancelled → loop gone");
            Ok(())
        })();
        if result.is_err() {
            let text = ui.text();
            let lines: Vec<&str> = text.lines().collect();
            println!("--- screen ---\n{}", lines[..lines.len().min(20)].join("\n"));
        }
        ui.send(b"\x1d").ok();
        ui.send(b"q").ok();
        let _ = ui.wait("quit", Duration::from_secs(5), |t| t.contains("Stop all and quit"));
        ui.send(b"\x1b[C\r").ok();
        let _ = ui.wait_exit(Duration::from_secs(20));
        if let Ok(conn) = crate::client::conn::try_connect(&instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_secs(3));
        }
        let _ = std::fs::remove_dir_all(&root);
        result.unwrap();
    }
}

#[cfg(test)]
mod plugin_probe {
    use std::time::Duration;

    /// Manual: `PROBE_PLUGIN=<dir> cargo test --release plugin_probe -- --ignored --nocapture`.
    /// Starts Copilot (sandboxed, no prompt) with `--plugin-dir` and lists skills.
    #[test]
    #[ignore]
    fn plugin_probe() {
        let plugin = std::env::var("PROBE_PLUGIN").unwrap();
        let root = std::env::temp_dir().join(format!("cc-plg-{}", uuid::Uuid::new_v4()));
        let (copilot_home, work) = (root.join("copilot"), root.join("work"));
        let real = dirs::home_dir().unwrap().join(".copilot");
        std::fs::create_dir_all(&copilot_home).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::copy(real.join("config.json"), copilot_home.join("config.json")).unwrap();
        let _ = std::fs::copy(real.join("settings.json"), copilot_home.join("settings.json"));
        let exe = crate::copilot_cfg::copilot_exe().unwrap();
        let mut args: Vec<String> = vec!["--plugin-dir".into(), plugin.clone(), "--disable-builtin-mcps".into()];
        for name in crate::copilot_cfg::snapshot().mcp.into_iter().map(|s| s.name) {
            args.extend(["--disable-mcp-server".into(), name]);
        }
        let off = std::path::PathBuf::from("false");
        let env: Vec<(&str, &std::path::Path)> = vec![("COPILOT_HOME", copilot_home.as_path()), ("COPILOT_AUTO_UPDATE", off.as_path())];
        std::env::set_current_dir(&work).unwrap();
        let mut ui = super::Console::start_full(&exe, &args, &env, 160, 50, true).unwrap();
        let _ = ui.wait("ready", Duration::from_secs(90), |t| t.contains("/ commands"));
        std::thread::sleep(Duration::from_secs(3));
        ui.send(b"/skills").unwrap();
        std::thread::sleep(Duration::from_millis(400));
        ui.send(b"\r").unwrap();
        std::thread::sleep(Duration::from_secs(4));
        let text = ui.text();
        println!("skill listed: {}", text.contains("canvas-probe-skill"));
        for line in text.lines().filter(|l| l.to_lowercase().contains("canvas-probe") || l.contains("plugin")) {
            println!("  {}", line.trim());
        }
        let _ = ui.send(b"\x1b");
        let _ = ui.send(b"\x03");
        std::thread::sleep(Duration::from_millis(500));
        let _ = ui.send(b"\x03");
        let _ = ui.wait_exit(Duration::from_secs(10));
        std::env::set_current_dir(std::env::temp_dir()).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod passive_probe {
    use std::io::Write;
    use std::time::Duration;

    use anyhow::Context;

    /// Manual (real Copilot, ~2 premium requests, sandboxed):
    /// `cargo test --release passive_probe -- --ignored --nocapture`.
    /// A passive session made in the UI runs on an emitted event, reports a
    /// status, messages the active session (typed in when idle), and a git
    /// push in the active session's history is detected as pr.pushed.
    #[test]
    #[ignore]
    fn passive_probe() {
        let root = std::env::temp_dir().join(format!("cc-psv-{}", uuid::Uuid::new_v4()));
        let (copilot_home, home, work, folder) = (root.join("copilot"), root.join("canvas"), root.join("work"), root.join("group"));
        let real = dirs::home_dir().unwrap().join(".copilot");
        let sessions = copilot_home.join("session-state");
        for d in [&sessions, &home, &work, &folder] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::copy(real.join("config.json"), copilot_home.join("config.json")).unwrap();
        let _ = std::fs::copy(real.join("settings.json"), copilot_home.join("settings.json"));
        let off: Vec<String> = crate::copilot_cfg::snapshot().mcp.into_iter().map(|s| s.name).collect();
        let active = uuid::Uuid::new_v4().to_string();
        let mut doc = crate::client::doc::Doc::default();
        doc.mcp.last_off = off.clone();
        doc.groups.push(crate::client::doc::Group { id: "g-probe".into(), name: "Probe group".into(), x: 0, y: 0, w: 70, h: 16, color: 0, folder: Some(folder.display().to_string()), source: None, defaults: None });
        doc.nodes.push(crate::client::doc::Node {
            id: active.clone(), kind: crate::client::doc::NodeKind::Session, name: "Active probe".into(), x: 2, y: 2,
            cwd: Some(work.display().to_string()), parent: None, agent_key: None, source: None, command: None, fresh: true, automation: None, synced_name: None, reviewed_ms: None,
            mcp_off: Some(off.clone()), passive: None, no_messages: false,
        });
        std::fs::write(home.join("canvas.json"), serde_json::to_string(&doc).unwrap()).unwrap();
        let instance = crate::paths::Instance::new(Some(format!("psv{}", std::process::id())), Some(home.clone()), Some(sessions.clone())).unwrap();
        let exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"target\release\copilot-canvas.exe");
        let args: Vec<String> = vec!["--instance".into(), instance.name.clone(), "--home".into(), home.to_string_lossy().into_owned(), "--sessions-dir".into(), sessions.to_string_lossy().into_owned(), "--cwd".into(), work.to_string_lossy().into_owned()];
        let mut ui = super::Console::start_with_env(&exe, &args, &[("COPILOT_HOME", copilot_home.as_path())], 210, 56).unwrap();
        let result = (|| -> anyhow::Result<()> {
            ui.wait("canvas", Duration::from_secs(20), |t| t.contains("Probe group") && t.contains("Active probe"))?;
            println!("1. create a passive session from the group menu");
            let (gx, gy) = ui.find_in("Probe group", 34, u16::MAX).context("group title")?;
            ui.send(format!("\x1b[<2;{};{}M\x1b[<2;{};{}m", gx + 1, gy + 1, gx + 1, gy + 1).as_bytes())?;
            ui.wait("group menu", Duration::from_secs(5), |t| t.contains("New passive session…"))?;
            let (mx, my) = ui.find("New passive session…").context("menu item")?;
            ui.click(mx, my)?;
            ui.wait("templates", Duration::from_secs(5), |t| t.contains("Drift guard") && t.contains("Rollout tracker"))?;
            ui.send(b"Custom")?;
            ui.send(b"\r")?;
            ui.wait("name prompt", Duration::from_secs(5), |t| t.contains("Name the Custom passive session"))?;
            ui.send(b"\x15Probe watcher\r")?;
            ui.wait("detail panel", Duration::from_secs(10), |t| t.contains("⟲ Probe watcher") && t.contains("Events…"))?;
            let (ex, ey) = ui.find("Events…").context("events button")?;
            ui.click(ex + 1, ey)?;
            ui.wait("events prompt", Duration::from_secs(5), |t| t.contains("Events it reacts to"))?;
            ui.send(b"\x15probe.ping\r")?;
            ui.wait("subscribed", Duration::from_secs(5), |t| t.contains("on probe.ping"))?;
            let canvas: crate::client::doc::Doc = loop {
                std::thread::sleep(Duration::from_millis(500));
                let d: crate::client::doc::Doc = serde_json::from_str(&std::fs::read_to_string(home.join("canvas.json"))?)?;
                if d.nodes.iter().any(|n| n.passive.as_ref().is_some_and(|p| p.subscribe == ["probe.ping"])) {
                    break d;
                }
            };
            let passive = canvas.nodes.iter().find(|n| n.passive.is_some()).context("passive node")?;
            let slug = passive.passive.as_ref().unwrap().slug.clone();
            let rules = folder.join(".canvas").join("passive").join(&slug).join("rules.md");
            anyhow::ensure!(rules.is_file() && folder.join("ai-docs").join("GOALS.md").is_file() && folder.join(".canvas").join("events.yaml").is_file(), "files scaffolded");
            std::fs::write(&rules, "# Probe watcher\n\nOn probe.ping run exactly these two commands, nothing else, then stop:\n1. copilot-canvas status ok \"pong N\"  (N is the n field of the event data)\n2. copilot-canvas send --to \"Active probe\" \"pong N\"\n")?;

            println!("2. open the active session (it receives messages when idle)");
            let (ax, ay) = ui.locate("Active probe", super::CANVAS)?;
            ui.click(ax, ay)?;
            ui.wait("active session ready", Duration::from_secs(120), |t| t.contains("/ commands"))?;
            ui.send(b"\x1d")?;
            std::thread::sleep(Duration::from_secs(3));

            println!("3. the active session emits probe.ping (as its skill would)");
            let status = std::process::Command::new(&exe)
                .args(["emit", "probe.ping", "--data", r#"{"n": 7}"#])
                .env("COPILOT_CANVAS_HOME", &home)
                .env("COPILOT_CANVAS_SESSION", &active)
                .status()?;
            anyhow::ensure!(status.success(), "emit failed");
            let started = std::time::Instant::now();
            ui.wait("passive ran and reported", Duration::from_secs(240), |t| t.contains("pong 7") && t.contains("ok ·"))?;
            println!("   passive reported \"ok · pong 7\" after {:.0}s", started.elapsed().as_secs_f32());
            ui.wait("message typed into the active session", Duration::from_secs(120), |t| t.contains("[From Probe watcher via Copilot Canvas] pong 7"))?;
            println!("   message delivered into the active session");

            println!("4. a git push in the active session's history becomes pr.pushed");
            let events = sessions.join(&active).join("events.jsonl");
            let now = crate::paths::rfc3339_now();
            let mut f = std::fs::OpenOptions::new().append(true).open(&events)?;
            writeln!(f, r#"{{"type":"tool.execution_start","timestamp":"{now}","data":{{"toolCallId":"probe-push","toolName":"powershell","arguments":{{"command":"git push origin feature"}}}}}}"#)?;
            writeln!(f, r#"{{"type":"tool.execution_complete","timestamp":"{now}","data":{{"toolCallId":"probe-push","success":true,"shellExecution":{{"exitCode":0}},"result":"To https://example/repo\n   1111111..2222222  feature -> feature"}}}}"#)?;
            drop(f);
            let (px, py) = ui.locate("Probe watcher", super::CANVAS)?;
            ui.click(px, py)?;
            ui.wait("pr.pushed in the group's recent events", Duration::from_secs(20), |t| t.contains("pr.pushed · Active probe"))?;
            println!("   detected pr.pushed from the active session");
            Ok(())
        })();
        if result.is_err() {
            let text = ui.text();
            println!("--- screen ---\n{text}");
            let log = std::fs::read_to_string(instance.log_path()).unwrap_or_default();
            println!("--- service log ---\n{}", log.lines().rev().take(25).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"));
        }
        ui.send(b"\x1b").ok();
        ui.send(b"\x1d").ok();
        ui.send(b"q").ok();
        let _ = ui.wait("quit", Duration::from_secs(5), |t| t.contains("Stop all and quit"));
        ui.send(b"\x1b[C\r").ok();
        let _ = ui.wait_exit(Duration::from_secs(20));
        if let Ok(conn) = crate::client::conn::try_connect(&instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_secs(4));
        }
        if result.is_ok() {
            let _ = std::fs::remove_dir_all(&root);
        } else {
            println!("artifacts kept in {}", root.display());
        }
        result.unwrap();
    }
}

#[cfg(test)]
mod factory_probe {
    use std::time::Duration;

    use anyhow::Context;

    fn sandbox(tag: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("cc-{tag}-{}", uuid::Uuid::new_v4()));
        let (copilot_home, home, sessions) = (root.join("copilot"), root.join("canvas"), root.join("copilot").join("session-state"));
        for d in [&sessions, &home] {
            std::fs::create_dir_all(d).unwrap();
        }
        let real = dirs::home_dir().unwrap().join(".copilot");
        std::fs::copy(real.join("config.json"), copilot_home.join("config.json")).unwrap();
        let _ = std::fs::copy(real.join("settings.json"), copilot_home.join("settings.json"));
        (root, copilot_home, home, sessions)
    }

    fn quit(ui: &mut super::Console, instance: &crate::paths::Instance) {
        ui.send(b"\x1b").ok();
        ui.send(b"q").ok();
        let _ = ui.wait("quit", Duration::from_secs(5), |t| t.contains("Stop all and quit") || t.contains("Quit"));
        ui.send(b"\x1b[C\r").ok();
        let _ = ui.wait_exit(Duration::from_secs(20));
        if let Ok(conn) = crate::client::conn::try_connect(instance) {
            let _ = conn.tx.send(crate::protocol::ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_secs(3));
        }
    }

    /// Manual, read-only (no Copilot): your real automations on a sandbox canvas.
    /// `PROBE_AUTOMATIONS=<manifest folder> cargo test --release factory_canvas -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn factory_canvas() {
        let dir = std::env::var("PROBE_AUTOMATIONS").expect("set PROBE_AUTOMATIONS to a manifest folder");
        let (root, copilot_home, home, sessions) = sandbox("fac");
        let doc = crate::client::doc::Doc { automations_dir: Some(dir), doctor_created: true, ..Default::default() };
        std::fs::write(home.join("canvas.json"), serde_json::to_string(&doc).unwrap()).unwrap();
        let instance = crate::paths::Instance::new(Some(format!("fac{}", std::process::id())), Some(home.clone()), Some(sessions.clone())).unwrap();
        let exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"target\release\copilot-canvas.exe");
        let args: Vec<String> = vec!["--instance".into(), instance.name.clone(), "--home".into(), home.to_string_lossy().into_owned(), "--sessions-dir".into(), sessions.to_string_lossy().into_owned()];
        let mut ui = super::Console::start_with_env(&exe, &args, &[("COPILOT_HOME", copilot_home.as_path())], 230, 64).unwrap();
        let result = (|| -> anyhow::Result<()> {
            ui.wait("automations on the canvas", Duration::from_secs(40), |t| t.contains("ExcelTeams-Morning") && t.contains("Excel Teams pipeline"))?;
            std::thread::sleep(Duration::from_secs(2));
            println!("--- canvas ---\n{}", ui.text());
            let (x, y) = ui.find_in("◷ ExcelTeams-Morning", 34, u16::MAX).context("Morning box")?;
            ui.click(x, y)?;
            ui.wait("info page", Duration::from_secs(10), |t| t.contains("◷ ExcelTeams-Morning") && t.contains("Runs "))?;
            std::thread::sleep(Duration::from_millis(500));
            println!("--- info page ---\n{}", ui.text());
            Ok(())
        })();
        if result.is_err() {
            println!("--- screen ---\n{}", ui.text());
        }
        quit(&mut ui, &instance);
        let _ = std::fs::remove_dir_all(&root);
        result.unwrap();
    }

    /// Manual (real Copilot, ~1-2 premium requests; registers and removes a
    /// temporary scheduled task): `cargo test --release factory_probe -- --ignored --nocapture`.
    /// A failing automation pulses on the canvas, the default doctor is created
    /// for the automations group, wakes on automation.failed and reports.
    #[test]
    #[ignore]
    fn factory_probe() {
        let (root, copilot_home, home, sessions) = sandbox("fpr");
        let (dir, state) = (root.join("automations"), root.join("state"));
        std::fs::create_dir_all(dir.join("probe")).unwrap();
        let task = format!("CanvasProbe-{}", std::process::id());
        let python = std::path::Path::new(&std::env::var("LOCALAPPDATA").unwrap()).join(r"Programs\Python\Python314\pythonw.exe");
        std::fs::write(
            dir.join("automations.yaml"),
            format!("launcher: 'Q:\\repos\\Automations\\scripts\\run_background_task.py'\npython: '{}'\nstate: '{}'\n", python.display(), state.display()),
        )
        .unwrap();
        std::fs::write(
            dir.join("probe").join(format!("{task}.yaml")),
            format!("name: '{task}'\nfamily: 'Probe family'\ncommand: 'C:\\Windows\\System32\\cmd.exe'\nargs: '/c exit 3'\ncwd: '{}'\nschedule:\n  - 'daily 03:17'\ntime_limit: '5m'\n", root.display()),
        )
        .unwrap();
        let exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(r"target\release\copilot-canvas.exe");
        let applied = std::process::Command::new(&exe).args(["automations", "apply", "--yes", "--dir"]).arg(&dir).output().unwrap();
        println!("{}", String::from_utf8_lossy(&applied.stdout));
        assert!(applied.status.success(), "{}", String::from_utf8_lossy(&applied.stderr));
        let off: Vec<String> = crate::copilot_cfg::snapshot().mcp.into_iter().map(|s| s.name).collect();
        let mut doc = crate::client::doc::Doc { automations_dir: Some(dir.display().to_string()), ..Default::default() };
        doc.mcp.last_off = off;
        std::fs::write(home.join("canvas.json"), serde_json::to_string(&doc).unwrap()).unwrap();
        let instance = crate::paths::Instance::new(Some(format!("fpr{}", std::process::id())), Some(home.clone()), Some(sessions.clone())).unwrap();
        let args: Vec<String> = vec!["--instance".into(), instance.name.clone(), "--home".into(), home.to_string_lossy().into_owned(), "--sessions-dir".into(), sessions.to_string_lossy().into_owned()];
        let mut ui = super::Console::start_with_env(&exe, &args, &[("COPILOT_HOME", copilot_home.as_path())], 210, 56).unwrap();
        let result = (|| -> anyhow::Result<()> {
            println!("1. the automation appears in its family and the doctor is added");
            ui.wait("automation and doctor", Duration::from_secs(40), |t| t.contains(&task) && t.contains("Probe family") && t.contains("⟲ Doctor"))?;
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let canvas: crate::client::doc::Doc = loop {
                let d: crate::client::doc::Doc = serde_json::from_str(&std::fs::read_to_string(home.join("canvas.json"))?)?;
                if d.nodes.iter().any(|n| n.passive.is_some()) || std::time::Instant::now() > deadline {
                    break d;
                }
                std::thread::sleep(Duration::from_millis(300));
            };
            let doctor = canvas.nodes.iter().find(|n| n.passive.is_some()).context("doctor node")?;
            let passive = doctor.passive.as_ref().unwrap();
            anyhow::ensure!(passive.subscribe == ["automation.failed", "automation.check"], "doctor subscriptions {:?}", passive.subscribe);
            let folder = canvas.group(&passive.group).and_then(|g| g.folder.clone()).context("automations group folder")?;
            let rules = crate::bus::passive_paths(std::path::Path::new(&folder), &passive.slug).rules;
            anyhow::ensure!(std::fs::read_to_string(&rules)?.contains("Automation doctor"), "doctor rules scaffolded");
            std::fs::write(&rules, "# Probe doctor\n\nOn automation.failed run exactly this command, nothing else, then stop:\ncopilot-canvas status alert \"saw NAME exit EXIT\"  (NAME and EXIT from the event data: name, exit)\n")?;
            std::thread::sleep(Duration::from_secs(3));

            println!("2. the task fails: its box pulses and the toolbar counts it");
            crate::taskschd::run(&task)?;
            ui.wait("failure shown", Duration::from_secs(60), |t| t.contains("last run failed") && t.contains("1 automation failing"))?;
            println!("3. the service wakes the doctor, which reports");
            let started = std::time::Instant::now();
            ui.wait("doctor reported", Duration::from_secs(300), |t| t.contains(&format!("saw {task} exit 3")))?;
            println!("   doctor reported after {:.0}s", started.elapsed().as_secs_f32());
            let log = std::fs::read_to_string(instance.log_path()).unwrap_or_default();
            anyhow::ensure!(log.contains(&format!("automation.failed {task}: exit code 3")), "service logged the failure");

            println!("4. the info page shows the failed run");
            let (x, y) = ui.find_in(&format!("◷ {task}"), 34, u16::MAX).context("task box")?;
            ui.click(x, y)?;
            ui.wait("info page", Duration::from_secs(10), |t| t.contains("exit code 3") && t.contains("1 of 1 failed") && t.contains("Ask doctor"))?;
            Ok(())
        })();
        if result.is_err() {
            println!("--- screen ---\n{}", ui.text());
            let log = std::fs::read_to_string(instance.log_path()).unwrap_or_default();
            println!("--- service log ---\n{}", log.lines().rev().take(25).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"));
        }
        quit(&mut ui, &instance);
        let _ = crate::taskschd::delete(&task);
        if result.is_ok() {
            let _ = std::fs::remove_dir_all(&root);
        } else {
            println!("artifacts kept in {}", root.display());
        }
        result.unwrap();
    }
}
