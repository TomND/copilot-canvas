//! The TUI client: canvas + sidebar + terminal panes, connected to the service.

mod input;
mod stable;

use canvas_core::{app, conn, doc, ui};
use canvas_core::input::InputEvent;

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{bounded, never, select, unbounded};
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste, EnableFocusChange, EnableMouseCapture,
};
use ratatui::crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ratatui::crossterm::{execute, queue};

use canvas_core::paths::Instance;
use app::App;
use doc::Store;

pub struct Options {
    pub instance: Instance,
    pub demo: bool,
    /// Shows frame rate, draw time and output-to-screen latency in the status bar.
    pub perf: bool,
    pub canvas: Option<PathBuf>,
    pub cwd: String,
    /// `--remote`: `loopback` or `HOST:PORT` (TCP); `None`: the local pipe.
    pub remote: Option<String>,
}

/// ratatui renders into this buffer; the bytes go to the console on the
/// output thread, so a slow console never blocks the UI thread.
#[derive(Clone, Default)]
struct FrameBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl FrameBuf {
    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl Write for FrameBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A finished frame on its way to the console, with the moments the input /
/// terminal output it shows arrived (for --perf).
struct Outgoing {
    bytes: Vec<u8>,
    input_since: Option<Instant>,
    output_since: Option<Instant>,
}

struct Presented {
    console: Duration,
    input_since: Option<Instant>,
    output_since: Option<Instant>,
}

/// Writes frames to the console (one write each). Runs until the sender closes.
fn spawn_console_writer(frames: crossbeam_channel::Receiver<Outgoing>, done: crossbeam_channel::Sender<Presented>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("console-out".into())
        .spawn(move || {
            let stdout = std::io::stdout();
            for frame in frames {
                let started = Instant::now();
                let mut out = stdout.lock();
                let _ = out.write_all(&frame.bytes).and_then(|_| out.flush());
                drop(out);
                let _ = done.send(Presented { console: started.elapsed(), input_since: frame.input_since, output_since: frame.output_since });
            }
        })
        .expect("console output thread")
}

/// Longest the UI thread spends on service messages before handling input again.
const SERVICE_BUDGET: Duration = Duration::from_millis(4);
/// Frames caused only by background updates (busy terminals, spinners) are
/// capped at 60 per second; nothing on screen needs more, and it spares the
/// terminal. Frames for your input are drawn immediately.
/// Full in-place repaint interval (heals any cells the terminal got wrong).
const REPAINT_EVERY: Duration = Duration::from_secs(10);
/// A console write this slow means the system is under load; repaint soon after.
const SLOW_CONSOLE: Duration = Duration::from_millis(100);
const BACKGROUND_FRAME: Duration = Duration::from_millis(16);
const TICK: Duration = Duration::from_millis(40);

pub fn run(options: Options) -> Result<()> {
    let home = options.instance.home.clone();
    let canvas_path = options.canvas.clone().unwrap_or_else(|| home.join(if options.demo { "demo-canvas.json" } else { "canvas.json" }));
    let store = Store::open(&canvas_path)?;
    let doc = match store.load()? {
        Some(doc) => doc,
        None if options.demo => canvas_core::demo::demo_doc(&options.cwd),
        None => doc::migrate_v1(&home.join("layout.json")).unwrap_or_default(),
    };
    let target = conn::ConnTarget::parse(options.remote.as_deref(), &options.instance)?;
    let conn_options = conn::ConnOptions {
        start: true,
        compress: canvas_core::settings::Settings::load(&home).compress(),
        name: "tui".into(),
        ..Default::default()
    };
    let session = conn::ClientSession::new();
    let mut conn = conn::open(&target, &conn_options, &session)?;
    let outdated = !target.is_remote() && conn.service_build != canvas_core::paths::build_id();
    // Terminals that were running in a replaced service; they are resumed.
    let mut resume = Vec::new();
    if target.is_remote() {
        // Remote clients never restart or upgrade the service.
        if conn.protocol_mismatch {
            bail!(
                "{} speaks protocol {} and this client {}; update this client",
                target.describe(),
                conn.protocol,
                canvas_core::protocol::PROTOCOL
            );
        }
    } else if conn.protocol_mismatch {
        (conn, resume) = upgrade_service(&options.instance, conn, true)?;
    } else if outdated {
        // Same protocol, older build: terminal fixes only apply after a restart.
        (conn, resume) = upgrade_service(&options.instance, conn, false)?;
    }
    // Replaced an older service: undo the `mouse: false` it forced on Copilot (when installed).
    let copilot = canvas_core::agents::AgentKind::Copilot.provider().program().is_ok();
    let mouse_restored =
        outdated && !options.demo && copilot && conn.service_build == canvas_core::paths::build_id() && canvas_core::copilot_cfg::restore_mouse().unwrap_or(false);

    let previous_hook = std::panic::take_hook();
    let crash_log = home.join("crash.log");
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableFocusChange, DisableBracketedPaste);
        let _ = std::io::stdout().write_all(b"\x1b[?7h");
        ratatui::restore();
        // Keep the details: the console that showed them is usually reused.
        let thread = std::thread::current();
        let report = format!(
            "{} build {} thread {:?}\n{info}\n{}\n\n",
            canvas_core::paths::now_ms(),
            canvas_core::paths::build_id(),
            thread.name().unwrap_or("?"),
            std::backtrace::Backtrace::force_capture()
        );
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&crash_log) {
            let _ = file.write_all(report.as_bytes());
        }
        previous_hook(info);
        eprintln!("Copilot Canvas crashed; details saved to {}. Your sessions keep running: start it again to reattach.", crash_log.display());
        // A panic on a helper thread would leave the UI half-alive with the
        // terminal already restored; exit instead (sessions live in the service).
        std::process::exit(101);
    }));
    canvas_core::win::raise_priority();
    ratatui::crossterm::terminal::enable_raw_mode().context("cannot initialize the terminal")?;
    execute!(std::io::stdout(), ratatui::crossterm::terminal::EnterAlternateScreen)?;
    // Auto-wrap off (DECAWM): writing the bottom-right cell must not scroll the
    // screen. Every cell is positioned explicitly, so nothing relies on wrapping.
    std::io::stdout().write_all(b"\x1b[?7l")?;
    std::io::stdout().flush()?;
    // A fixed viewport: the size comes from resize events, so drawing never
    // queries the console (which can block while it is busy).
    let (cols, rows) = ratatui::crossterm::terminal::size().context("cannot read the terminal size")?;
    let frame_buf = FrameBuf::default();
    let mut terminal = ratatui::Terminal::with_options(
        stable::StableBackend::new(frame_buf.clone()),
        ratatui::TerminalOptions { viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, cols, rows)) },
    )
    .context("cannot initialize the terminal")?;
    // A fixed viewport is not cleared automatically; start from a blank screen.
    terminal.clear()?;
    let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableFocusChange, EnableBracketedPaste);

    let result = (|| -> Result<()> {
        let (input_tx, input_rx) = bounded(256);
        input::spawn(input_tx);
        let (app_tx, app_rx) = unbounded();
        let mut service_rx = conn.rx.clone();
        let mut app = App::new(conn, store, doc, options.demo, options.cwd.clone(), app_tx, home.clone());
        app.instance = Some(options.instance.clone());
        app.target = Some(target);
        app.conn_options = conn_options;
        if !resume.is_empty() {
            let count = app.resume_terminals(&resume);
            app.notify(format!("Terminal service updated; resumed {count} session(s)."), false);
        }
        if mouse_restored {
            app.notify("Copilot's mouse setting was off (forced by the old service); it is on again, so the wheel scrolls smoothly.", false);
        }
        if options.perf {
            app.perf = Some(app::Perf::default());
        }
        let mut last_draw = Instant::now() - Duration::from_secs(1);
        let mut last_tick = Instant::now();
        let (frame_tx, frame_rx) = bounded::<Outgoing>(1);
        let (done_tx, done_rx) = bounded::<Presented>(4);
        let writer = spawn_console_writer(frame_rx, done_tx);
        // At most one frame in flight: while the console is busy, the UI keeps
        // handling input and messages and the next frame shows the latest state.
        let mut in_flight = false;
        // Self-healing: a full in-place repaint every few seconds, soon after a
        // slow console write (a sign of load), and on Ctrl+L.
        let mut next_repaint = Instant::now() + REPAINT_EVERY;
        let mut repaint_soon = false;
        let mut last_repaint = Instant::now();
        // Set when input arrived since the last frame (draw at once, not paced).
        let mut input_waiting = false;
        let mut resize: Option<(u16, u16)> = None;
        let on_input = |app: &mut App, batch: Vec<InputEvent>, resize: &mut Option<(u16, u16)>| {
            for event in &batch {
                if let InputEvent::Resize(w, h) = event {
                    *resize = Some((*w, *h));
                }
            }
            if let Some(perf) = &mut app.perf {
                perf.input_pending.get_or_insert_with(Instant::now);
            }
            app.on_input(batch);
        };
        loop {
            // Input first: queued service updates never delay what you do.
            while let Ok(batch) = input_rx.try_recv() {
                on_input(&mut app, batch, &mut resize);
                input_waiting = true;
            }
            if let Some((w, h)) = resize.take() {
                terminal.resize(ratatui::layout::Rect::new(0, 0, w, h))?;
                app.needs_draw = true;
            }
            if Instant::now() >= next_repaint || std::mem::take(&mut app.repaint_requested) {
                repaint_soon = true;
            }
            if repaint_soon && !in_flight {
                app.needs_draw = true;
            }
            let gap = if input_waiting { Duration::ZERO } else { BACKGROUND_FRAME };
            if app.needs_draw && !in_flight && last_draw.elapsed() >= gap {
                input_waiting = false;
                if std::mem::take(&mut repaint_soon) {
                    // Make ratatui believe every cell differs from the screen: render a
                    // throwaway frame and discard its bytes. The next frame then
                    // rewrites every cell in place (no clear, so no flicker).
                    terminal.draw(|frame| {
                        for cell in frame.buffer_mut().content.iter_mut() {
                            cell.set_symbol("\u{E000}");
                        }
                    })?;
                    let _ = frame_buf.take();
                    last_repaint = Instant::now();
                    next_repaint = last_repaint + REPAINT_EVERY;
                }
                let started = Instant::now();
                // Synchronized output: the terminal presents each frame whole (no tearing).
                queue!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
                let mut compose = Duration::ZERO;
                terminal.draw(|frame| {
                    let t = Instant::now();
                    ui::draw(&mut app, frame);
                    compose = t.elapsed();
                })?;
                queue!(terminal.backend_mut(), EndSynchronizedUpdate)?;
                let mut bytes = frame_buf.take();
                if !app.raw_out.is_empty() {
                    let mut raw = std::mem::take(&mut app.raw_out);
                    raw.extend_from_slice(&bytes);
                    bytes = raw;
                }
                app.needs_draw = false;
                last_draw = Instant::now();
                let (mut input_since, mut output_since) = (None, None);
                if let Some(perf) = &mut app.perf {
                    perf.drawn(started, last_draw, bytes.len() as u64, compose);
                    input_since = perf.input_pending.take();
                    output_since = perf.pending.take();
                }
                in_flight = true;
                if frame_tx.send(Outgoing { bytes, input_since, output_since }).is_err() {
                    break;
                }
                let t = Instant::now();
                app.after_draw();
                if let Some(perf) = &mut app.perf {
                    perf.phase("after-draw", t.elapsed());
                }
            }
            let timeout = if app.needs_draw && !in_flight { gap.saturating_sub(last_draw.elapsed()) } else { TICK.saturating_sub(last_tick.elapsed()) };
            select! {
                recv(done_rx) -> presented => {
                    in_flight = false;
                    if presented.as_ref().is_ok_and(|p| p.console > SLOW_CONSOLE) && last_repaint.elapsed() > Duration::from_secs(2) {
                        repaint_soon = true;
                    }
                    if let (Ok(presented), Some(perf)) = (presented, &mut app.perf) {
                        perf.presented(presented.console, presented.input_since, presented.output_since);
                    }
                }
                recv(input_rx) -> batch => match batch {
                    Ok(batch) => {
                        on_input(&mut app, batch, &mut resize);
                        // Input latency matters more than frame pacing.
                        input_waiting = true;
                    }
                    Err(_) => break,
                },
                recv(service_rx) -> message => match message {
                    Ok(message) => {
                        if let Some(perf) = &mut app.perf
                            && matches!(message, canvas_core::protocol::ServiceMsg::Frame(_) | canvas_core::protocol::ServiceMsg::Frame8(_))
                        {
                            perf.pending.get_or_insert_with(Instant::now);
                        }
                        let t = Instant::now();
                        app.on_service(message);
                        // Budget: leftovers wait for the next pass, after input and drawing.
                        while t.elapsed() < SERVICE_BUDGET
                            && let Ok(message) = service_rx.try_recv()
                        {
                            app.on_service(message);
                        }
                        if let Some(perf) = &mut app.perf {
                            perf.service_batch(t.elapsed());
                        }
                    }
                    Err(_) => {
                        service_rx = never();
                        // Reconnects in the background; input is blocked meanwhile.
                        if app.restarting.is_none() {
                            app.connection_lost();
                        }
                    }
                },
                recv(app_rx) -> event => {
                    if let Ok(event) = event {
                        app.on_app_event(event);
                    }
                    if let Some(rx) = app.new_service_rx.take() {
                        service_rx = rx;
                    }
                },
                default(timeout) => {}
            }
            if last_tick.elapsed() >= TICK {
                last_tick = Instant::now();
                app.on_tick();
            }
            if app.quit {
                break;
            }
        }
        drop(frame_tx);
        let _ = writer.join();
        app.save_now();
        if let Some(error) = &app.save_error {
            bail!("the canvas could not be saved: {error}");
        }
        Ok(())
    })();

    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableFocusChange, DisableBracketedPaste);
    // Auto-wrap back on; clear the tab/taskbar progress the canvas mirrored.
    let _ = std::io::stdout().write_all(b"\x1b[?7h\x1b]9;4;0;0\x07");
    ratatui::restore();
    let _ = std::io::stdout().flush();
    result
}

/// The running service is from another build (`required`: it speaks an older
/// protocol). Restart it, but never stop running sessions without asking.
fn upgrade_service(instance: &Instance, conn: conn::Conn, required: bool) -> Result<(conn::Conn, Vec<String>)> {
    use canvas_core::protocol::{ClientMsg, ServiceMsg};
    conn.tx.send(ClientMsg::Stats)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut alive = None;
    let mut running: Vec<String> = Vec::new();
    while Instant::now() < deadline && alive.is_none() {
        match conn.rx.recv_timeout(Duration::from_millis(200)) {
            Ok(ServiceMsg::Stats(stats)) => alive = Some(stats.alive),
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
    let alive = alive.context("the running terminal service did not answer")?;
    if alive > 0 {
        let why = if required { "speaks an older protocol and must be restarted" } else { "is from an older build; restart it to get this build's terminal fixes" };
        eprintln!(
            "The Copilot Canvas terminal service running {alive} live session(s) {why}.\n\
             Restarting stops those terminals (Copilot conversations stay resumable).\n\
             Restart it now? [y/N] "
        );
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            if required {
                bail!("kept the running service; quit its sessions or run `copilot-canvas service stop`, then reopen");
            }
            eprintln!("Kept it. Click ⟳ Update service in the toolbar any time; the sessions resume by themselves.");
            // Fresh connection: the probe above consumed the initial messages.
            drop(conn);
            return Ok((conn::connect(instance, true)?, Vec::new()));
        }
    }
    conn.tx.send(ClientMsg::Shutdown)?;
    drop(conn);
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && conn::try_connect(instance).is_ok() {
        std::thread::sleep(Duration::from_millis(300));
    }
    let conn = conn::connect(instance, true)?;
    anyhow::ensure!(!conn.protocol_mismatch, "the terminal service is still the old version");
    Ok((conn, running))
}
