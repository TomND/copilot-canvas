//! Copilot Canvas GPU client: the canvas, sidebar and terminals in a native
//! window (wgpu + winit + glyphon), on the same service and app core as the
//! TUI.

mod boxdraw;
mod canvas;
mod chrome;
mod gpu;
mod hooks;
mod input;
mod perf;
mod platform;
mod quads;
mod scene;
mod shell;
mod text;
mod theme;
mod tiles;

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use canvas_core::app::App;
use canvas_core::conn::{self, Conn, ConnOptions, ConnTarget};
use canvas_core::dialogs::{Action, Confirm, Modal};
use canvas_core::paths::{self, Instance};
use canvas_core::protocol::{ClientMsg, ServiceMsg};
use canvas_core::prefs::PrefsFile;
use canvas_core::remote::RemoteBackend;
use canvas_core::rpc::Backend;
use canvas_core::view::RenderMode;
use winit::event_loop::EventLoop;

const USAGE: &str = "\
Copilot Canvas — GPU client

USAGE
  copilot-canvas-gui [options]

OPTIONS
  --demo                safe demo with native PowerShell panes, no agent calls
  --perf                frame time, present latency and service round trip overlay
  --cwd DIR             default folder for new sessions (default: current folder)
  --canvas FILE         canvas file (default: <home>/canvas.json)
  --instance NAME       separate service + canvas namespace (default: default)
  --home DIR            data directory (default: ~/.copilot-canvas, or COPILOT_CANVAS_HOME)
  --sessions-dir DIR    Copilot session history to index
  --remote TARGET       connect over TCP: `loopback` or HOST:PORT
  --size COLSxROWS      initial window size in cells
  --test-hooks          automation pipe (input, screen, tree, PNG); prints its name
  -h, --help · -V, --version

The terminal service is copilot-canvas.exe next to this program.
";

struct Args {
    demo: bool,
    perf: bool,
    test_hooks: bool,
    cwd: Option<PathBuf>,
    canvas: Option<PathBuf>,
    instance: Option<String>,
    home: Option<PathBuf>,
    sessions_dir: Option<PathBuf>,
    remote: Option<String>,
    size: Option<(u16, u16)>,
}

fn parse() -> Result<Args> {
    let mut args = Args { demo: false, perf: false, test_hooks: false, cwd: None, canvas: None, instance: None, home: None, sessions_dir: None, remote: None, size: None };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        let mut value = |name: &str| iter.next().with_context(|| format!("{name} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("copilot-canvas-gui {}", paths::build_id());
                std::process::exit(0);
            }
            "--demo" => args.demo = true,
            "--perf" => args.perf = true,
            "--test-hooks" => args.test_hooks = true,
            "--cwd" => args.cwd = Some(PathBuf::from(value("--cwd")?)),
            "--canvas" => args.canvas = Some(PathBuf::from(value("--canvas")?)),
            "--instance" => args.instance = Some(value("--instance")?),
            "--home" => args.home = Some(PathBuf::from(value("--home")?)),
            "--sessions-dir" => args.sessions_dir = Some(PathBuf::from(value("--sessions-dir")?)),
            "--remote" => args.remote = Some(value("--remote")?),
            "--size" => {
                let size = value("--size")?;
                let parsed = size.split_once(['x', 'X']).and_then(|(c, r)| Some((c.trim().parse().ok()?, r.trim().parse().ok()?)));
                args.size = Some(parsed.filter(|(c, r): &(u16, u16)| *c >= 60 && *r >= 16).with_context(|| format!("--size takes COLSxROWS (at least 60x16), not {size:?}"))?);
            }
            other => bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }
    Ok(args)
}

fn main() {
    match real_main() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("copilot-canvas-gui: {error:#}");
            std::process::exit(1);
        }
    }
}

/// The service runs from `copilot-canvas.exe` next to this program; its
/// content hash is the build both sides compare. False when it is missing.
fn use_sibling_service() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let sibling = exe.with_file_name("copilot-canvas.exe");
    let found = sibling.is_file();
    if found {
        paths::set_build_exe(sibling);
    }
    found
}

/// How long the service may take to open the canvas.
const OPEN_TIMEOUT: Duration = Duration::from_secs(20);

fn real_main() -> Result<i32> {
    let args = parse()?;
    let service_exe = use_sibling_service();
    // System fonts load while the service connects and the window comes up.
    let fonts = std::thread::Builder::new().name("fonts".into()).spawn(text::load)?;
    let instance = Instance::new(args.instance.clone(), args.home.clone(), args.sessions_dir.clone())?;
    let home = instance.home.clone();
    let cwd = paths::absolute(&args.cwd.clone().map(Ok).unwrap_or_else(std::env::current_dir)?).to_string_lossy().into_owned();
    let target = ConnTarget::parse(args.remote.as_deref(), &instance)?;
    anyhow::ensure!(service_exe || matches!(target, ConnTarget::Tcp { .. }), "copilot-canvas.exe must be next to this program: it runs the terminal service");
    // Remote clients ask the service for everything; local ones run the backend in-process.
    let remote_backend = target.is_remote().then(RemoteBackend::new);
    let backend: Arc<dyn Backend> = match &remote_backend {
        Some(remote) => remote.clone(),
        None => Arc::new(canvas_agents::backend::LocalBackend::new(canvas_agents::backend::Ctx::new(&instance))),
    };
    let conn_options = ConnOptions {
        start: true,
        compress: canvas_core::settings::Settings::load(&home).compress(),
        name: "gui".into(),
        rpc: remote_backend,
        ..Default::default()
    };
    let session = conn::ClientSession::new();
    let mut conn = conn::open(&target, &conn_options, &session)?;
    let outdated = !target.is_remote() && conn.service_build != paths::build_id();
    // Terminals of a replaced service (resumed), or the question to ask.
    let mut resume = Vec::new();
    let mut ask: Option<(u32, bool)> = None;
    if target.is_remote() {
        // Remote clients never restart or upgrade the service.
        if conn.protocol_mismatch {
            bail!("{} speaks protocol {} and this client {}; update this client", target.describe(), conn.protocol, canvas_core::protocol::PROTOCOL);
        }
    } else if conn.protocol_mismatch || outdated {
        let (alive, running) = probe(&conn)?;
        if alive == 0 {
            conn = restart(&instance, conn, &target, &conn_options, &session)?;
            resume = running;
        } else {
            // Ask in the window; a fresh connection replaces the probed one.
            ask = Some((alive, conn.protocol_mismatch));
            drop(conn);
            conn = conn::open(&target, &conn_options, &session)?;
        }
    }
    // Replaced an older service: undo the `mouse: false` it forced on Copilot (when installed).
    let mouse_restored = outdated
        && !args.demo
        && conn.service_build == paths::build_id()
        && backend.info().is_ok_and(|info| info.installed.contains(&canvas_core::agents::AgentKind::Copilot))
        && backend.restore_mouse().unwrap_or(false);
    // The canvas belongs to the service: a local `--canvas` file is named
    // absolutely; a remote one is a path on the service's machine.
    let canvas = args.canvas.as_ref().map(|p| if target.is_remote() { p.clone() } else { paths::absolute(p) }).map(|p| p.display().to_string());
    let (opened, early) = conn::open_doc(&conn, canvas.clone(), args.demo, &cwd, OPEN_TIMEOUT).context("cannot open the canvas")?;
    let prefs = PrefsFile::path(&home, &instance.name, &opened.key);
    install_crash_log(home.join("crash.log"));
    canvas_core::win::raise_priority();

    let (app_tx, app_rx) = crossbeam_channel::unbounded();
    let mut app = App::new(conn, backend, opened, args.demo, cwd, app_tx, Some(prefs));
    app.doc_request = (canvas, args.demo);
    app.render_mode = RenderMode::Gpu;
    app.view_scale = canvas_core::app::ZOOMS[(app.doc.view.zoom as usize).min(2)].0 as f32 / canvas_core::app::ZOOMS[(app.doc.view.zoom as usize).min(2)].1 as f32;
    let platform = Arc::new(platform::GuiPlatform::default());
    app.platform = platform.clone();
    app.instance = Some(instance);
    app.target = Some(target);
    app.conn_options = conn_options;
    // What the service sent while the canvas was opening.
    for message in early {
        app.on_service(message);
    }
    if !resume.is_empty() {
        let count = app.resume_terminals(&resume);
        app.notify(format!("Terminal service updated; resumed {count} session(s)."), false);
    }
    if mouse_restored {
        app.notify("Copilot's mouse setting was off (forced by the old service); it is on again, so the wheel scrolls smoothly.", false);
    }
    match ask {
        Some((alive, true)) => app.modals.push(Modal::Confirm(Confirm {
            title: "Update the terminal service".into(),
            body: vec![
                format!("The terminal service running {alive} live session(s) speaks an older protocol and must be restarted."),
                "Restarting stops those terminals; they resume right after (conversations stay resumable).".into(),
            ],
            buttons: vec![("Restart and resume".into(), Action::RestartService), ("Quit (keep them running)".into(), Action::Detach)],
            sel: 0,
        })),
        Some((_, false)) => app.confirm_update_service(),
        None => {}
    }

    let event_loop = EventLoop::<shell::UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    if args.test_hooks {
        let name = hooks::pipe_name();
        hooks::serve(&name, proxy.clone())?;
        println!("{name}");
        let _ = std::io::stdout().flush();
    }
    let options = shell::Options { perf: args.perf, test_hooks: args.test_hooks, size: args.size };
    let mut shell = shell::Shell::new(app, options, proxy, fonts, platform, app_rx);
    event_loop.run_app(&mut shell)?;
    Ok(shell.exit_code)
}

/// Asks the running service how many sessions it runs (and which).
fn probe(conn: &Conn) -> Result<(u32, Vec<String>)> {
    conn.tx.send(ClientMsg::Stats)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut running: Vec<String> = Vec::new();
    while Instant::now() < deadline {
        match conn.rx.recv_timeout(Duration::from_millis(200)) {
            Ok(ServiceMsg::Stats(stats)) => return Ok((stats.alive, running)),
            Ok(ServiceMsg::Terminals(infos)) => {
                for info in infos {
                    running.retain(|id| *id != info.id);
                    if info.alive {
                        running.push(info.id);
                    }
                }
            }
            _ => {}
        }
    }
    bail!("the running terminal service did not answer")
}

/// Stops an idle service of another build and starts this build's.
fn restart(instance: &Instance, conn: Conn, target: &ConnTarget, options: &ConnOptions, session: &Arc<conn::ClientSession>) -> Result<Conn> {
    conn.tx.send(ClientMsg::Shutdown)?;
    drop(conn);
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && conn::try_connect(instance).is_ok() {
        std::thread::sleep(Duration::from_millis(300));
    }
    let conn = conn::open(target, options, session)?;
    anyhow::ensure!(!conn.protocol_mismatch, "the terminal service is still the old version");
    Ok(conn)
}

/// Panics are written to the instance's crash log; the process exits (the
/// sessions keep running in the service).
fn install_crash_log(path: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let report = format!(
            "{} gui build {} thread {:?}\n{info}\n{}\n\n",
            paths::now_ms(),
            paths::build_id(),
            thread.name().unwrap_or("?"),
            std::backtrace::Backtrace::force_capture()
        );
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = file.write_all(report.as_bytes());
        }
        previous(info);
        eprintln!("Copilot Canvas crashed; details saved to {}. Your sessions keep running: start it again to reattach.", path.display());
        std::process::exit(101);
    }));
}
