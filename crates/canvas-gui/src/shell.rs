//! The window and its event loop: winit events become core input, service
//! messages and app events wake the loop through the proxy, frames are drawn
//! only when something changed (input immediately, background updates paced
//! to the display), and ticks run every 40 ms while anything is active.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use canvas_core::app::{App, AppEvent, Focus};
use canvas_core::input::{InputEvent, KeyCode, KeyEvent, Modifiers, MouseButton, MouseEventKind};
use canvas_core::protocol::ServiceMsg;
use canvas_core::view::View;
use crossbeam_channel::{Receiver, Sender, unbounded};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use serde_json::{Value, json};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use crate::gpu::{Gpu, Skip, Surface};
use crate::perf::{Perf, ms};
use crate::platform::{GuiPlatform, Taskbar};
use crate::quads::Quad;
use crate::scene::{self, Painter, Scene};
use crate::text::{Fonts, Grid, ShapeCache, Spec};
use crate::theme::{self, BG};
use crate::tiles::{TileCache, TileInput};

/// Housekeeping interval while anything is active (as the TUI).
const TICK: Duration = Duration::from_millis(40);
/// … and while nothing is.
const IDLE_TICK: Duration = Duration::from_millis(400);
/// Longest the UI thread spends on service messages before handling input again.
const SERVICE_BUDGET: Duration = Duration::from_millis(4);

pub enum UserEvent {
    /// Service messages are waiting.
    Service,
    /// The service connection of this generation closed.
    ServiceClosed(u64),
    App(Box<AppEvent>),
    Hook(crate::hooks::Request),
}

pub struct Options {
    pub perf: bool,
    pub test_hooks: bool,
    /// Initial window size in cells (fixed with test hooks).
    pub size: Option<(u16, u16)>,
}

pub struct Shell {
    app: App,
    options: Options,
    proxy: EventLoopProxy<UserEvent>,
    platform: Arc<GuiPlatform>,
    taskbar: Taskbar,
    font_loader: Option<std::thread::JoinHandle<Fonts>>,
    fonts: Option<Fonts>,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    /// The window's swap chain (Phase 5: one per window, sharing `gpu`).
    surface: Option<Surface>,
    grid: Grid,
    cols: u16,
    rows: u16,
    cache: ShapeCache,
    tile_cache: TileCache,
    chrome: Buffer,
    overlay: Buffer,
    view: View,
    mods: ModifiersState,
    /// Pointer in fractional cells.
    pointer: (f32, f32),
    held: Vec<MouseButton>,
    wheel: crate::input::Wheel,
    preedit: Option<String>,
    ime_rect: Option<[i32; 4]>,
    hover: Option<(u16, u16)>,
    cursor_icon: Option<winit::window::CursorIcon>,
    next_tick: Instant,
    last_draw: Instant,
    input_waiting: bool,
    /// Present again even though the app has nothing new (resize, hover, retry).
    redraw: bool,
    retry_at: Option<Instant>,
    retries: u32,
    frame_interval: Duration,
    svc_gen: u64,
    svc_pending: Arc<AtomicBool>,
    svc_tx: Sender<(u64, ServiceMsg)>,
    svc_rx: Receiver<(u64, ServiceMsg)>,
    perf: Perf,
    stats_sent: Option<Instant>,
    /// Last keystroke into a terminal: its echo is drawn at once, not paced.
    typed_at: Option<Instant>,
    pub first_paint: Option<Duration>,
    pub exit_code: i32,
    exiting: bool,
}

impl Shell {
    pub fn new(
        app: App,
        options: Options,
        proxy: EventLoopProxy<UserEvent>,
        font_loader: std::thread::JoinHandle<Fonts>,
        platform: Arc<GuiPlatform>,
        app_rx: Receiver<AppEvent>,
    ) -> Self {
        let (svc_tx, svc_rx) = unbounded();
        let area = Rect::new(0, 0, 1, 1);
        let mut shell = Self {
            app,
            options,
            proxy: proxy.clone(),
            platform,
            taskbar: Taskbar::default(),
            font_loader: Some(font_loader),
            fonts: None,
            window: None,
            gpu: None,
            surface: None,
            grid: Grid::new(0.6, 1.0),
            cols: 0,
            rows: 0,
            cache: ShapeCache::default(),
            tile_cache: TileCache::default(),
            chrome: Buffer::empty(area),
            overlay: Buffer::empty(area),
            view: View::default(),
            mods: ModifiersState::empty(),
            pointer: (0.0, 0.0),
            held: Vec::new(),
            wheel: Default::default(),
            preedit: None,
            ime_rect: None,
            hover: None,
            cursor_icon: None,
            next_tick: Instant::now(),
            last_draw: Instant::now() - Duration::from_secs(1),
            input_waiting: true,
            redraw: true,
            retry_at: None,
            retries: 0,
            frame_interval: Duration::from_micros(16_667),
            svc_gen: 0,
            svc_pending: Arc::new(AtomicBool::new(false)),
            svc_tx,
            svc_rx,
            perf: Perf::default(),
            stats_sent: None,
            typed_at: None,
            first_paint: None,
            exit_code: 0,
            exiting: false,
        };
        let rx = shell.app.conn.rx.clone();
        shell.forward_service(rx);
        std::thread::Builder::new()
            .name("app-events".into())
            .spawn(move || {
                for event in app_rx {
                    if proxy.send_event(UserEvent::App(Box::new(event))).is_err() {
                        break;
                    }
                }
            })
            .expect("app event thread");
        shell
    }

    /// Forwards a service connection's messages to the UI thread.
    fn forward_service(&mut self, rx: Receiver<ServiceMsg>) {
        self.svc_gen += 1;
        let generation = self.svc_gen;
        let (tx, pending, proxy) = (self.svc_tx.clone(), self.svc_pending.clone(), self.proxy.clone());
        let _ = std::thread::Builder::new().name("service-forward".into()).spawn(move || {
            for message in rx {
                if tx.send((generation, message)).is_err() {
                    return;
                }
                if !pending.swap(true, Ordering::AcqRel) && proxy.send_event(UserEvent::Service).is_err() {
                    return;
                }
            }
            let _ = proxy.send_event(UserEvent::ServiceClosed(generation));
        });
    }

    fn drain_service(&mut self) {
        self.svc_pending.store(false, Ordering::Release);
        let started = Instant::now();
        let focused = self.app.focused_term.clone();
        while let Ok((generation, message)) = self.svc_rx.try_recv() {
            if generation != self.svc_gen {
                continue;
            }
            match &message {
                ServiceMsg::Frame8(frame) => self.on_frame(&frame.id, focused.as_deref()),
                ServiceMsg::Frame(frame) => self.on_frame(&frame.id, focused.as_deref()),
                ServiceMsg::Stats(_) => {
                    if let Some(sent) = self.stats_sent.take() {
                        self.perf.rtt_ms.push(ms(sent.elapsed()));
                    }
                }
                _ => {}
            }
            self.app.on_service(message);
            if started.elapsed() >= SERVICE_BUDGET {
                // Leftovers after input and drawing.
                if !self.svc_pending.swap(true, Ordering::AcqRel) {
                    let _ = self.proxy.send_event(UserEvent::Service);
                }
                break;
            }
        }
    }

    fn on_frame(&mut self, id: &str, focused: Option<&str>) {
        self.perf.output_at.get_or_insert_with(Instant::now);
        if focused == Some(id) {
            if self.perf.key_at.is_some() {
                self.perf.echoed = true;
            }
            // The echo of what you typed is drawn at once.
            if self.typed_at.is_some_and(|t| t.elapsed() < Duration::from_millis(250)) {
                self.input_waiting = true;
            }
        }
    }

    fn after_events(&mut self) {
        if let Some(rx) = self.app.new_service_rx.take() {
            self.forward_service(rx);
        }
    }

    /// Feeds input to the core (drawn at once, not paced).
    fn input(&mut self, events: Vec<InputEvent>) {
        let now = Instant::now();
        self.perf.input_at.get_or_insert(now);
        let typing = self.app.focus == Focus::Terminal && self.app.modals.is_empty();
        if typing && events.iter().any(|e| matches!(e, InputEvent::Key(_) | InputEvent::Text(_) | InputEvent::Paste(_))) {
            self.perf.key_at.get_or_insert(now);
            self.perf.echoed = false;
            self.typed_at = Some(now);
        }
        self.app.on_input(events);
        self.input_waiting = true;
        self.after_events();
    }

    /// Ctrl+V / Shift+Insert outside a terminal paste into the focused field
    /// (a terminal's own paste, with images, is the core's).
    fn gui_key(&mut self, key: &KeyEvent) -> bool {
        let terminal = self.app.focus == Focus::Terminal && self.app.modals.is_empty();
        if is_paste_key(key) && !terminal {
            if let Ok(text) = self.app.platform.clipboard_text() {
                self.input(vec![InputEvent::Paste(text)]);
            }
            return true;
        }
        false
    }

    fn surface_size(&self) -> (u32, u32) {
        match &self.window {
            Some(window) => {
                let size = window.inner_size();
                (size.width.max(1), size.height.max(1))
            }
            None => self.grid_size(),
        }
    }

    fn grid_size(&self) -> (u32, u32) {
        ((self.cols as f32 * self.grid.cell_w) as u32, (self.rows as f32 * self.grid.cell_h) as u32)
    }

    /// Lays out the cells for a window size (unless the grid is fixed).
    fn fit(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        let (cols, rows) = match (self.options.test_hooks, self.options.size) {
            (true, Some(size)) => size,
            _ => self.grid.cells(width, height),
        };
        if (cols, rows) != (self.cols, self.rows) {
            self.cols = cols;
            self.rows = rows;
            self.app.on_input(vec![InputEvent::Resize(cols, rows)]);
        }
        self.app.needs_draw = true;
    }

    /// Builds the view, draws it and presents.
    fn frame(&mut self) {
        if self.fonts.is_none() || self.cols == 0 {
            return;
        }
        let started = Instant::now();
        let area = Rect::new(0, 0, self.cols, self.rows);
        if self.chrome.area != area {
            self.chrome = Buffer::empty(area);
            self.overlay = Buffer::empty(area);
        } else {
            self.chrome.reset();
            self.overlay.reset();
        }
        self.view = canvas_core::ui::render(&mut self.app, &mut self.chrome, area, Some(&mut self.overlay));
        self.app.needs_draw = false;
        self.redraw = false;
        let viewed = Instant::now();
        let (width, height) = self.surface_size();
        let (scene, ime) = self.build_scene(width, height);
        let built = Instant::now();
        // Minimized: nothing to present (the view is still built for the hooks).
        let minimized = self.window.as_ref().is_some_and(|w| w.inner_size().width == 0 || w.inner_size().height == 0);
        let result = match (&mut self.gpu, &self.surface, &self.window, &mut self.fonts) {
            (Some(gpu), Some(surface), Some(window), Some(fonts)) if !minimized => Some(gpu.present(surface, window, &scene, &self.cache, fonts)),
            _ => None,
        };
        self.cache.end_frame();
        let now = Instant::now();
        self.last_draw = now;
        self.input_waiting = false;
        match result {
            Some(Ok(presented)) => {
                self.retries = 0;
                self.retry_at = None;
                self.perf.view_ms.push(ms(viewed - started));
                self.perf.scene_ms.push(ms(built - viewed));
                self.perf.encode_ms.push(ms(presented.encode));
                self.perf.acquire_ms.push(ms(presented.acquire));
                self.perf.present_ms.push(ms(presented.present));
                self.perf.frame_ms.push(ms(now - started - presented.acquire));
                self.perf.presented(now);
                if self.first_paint.is_none() {
                    trace("first present");
                }
                self.first_paint.get_or_insert_with(crate::perf::since_start);
            }
            Some(Err(Skip::Retry)) => {
                // Occluded or resized: try again, backing off while it lasts.
                self.perf.skipped += 1;
                self.retries += 1;
                self.redraw = true;
                let wait = if self.retries > 10 { 250 } else { 16 };
                self.retry_at = Some(now + Duration::from_millis(wait));
            }
            None => {}
        }
        self.app.after_draw();
        self.app.raw_out.clear();
        self.tile_cache.retain(|id| self.app.views.contains_key(id));
        // The IME candidate window follows the focused terminal's cursor or text field.
        let field = self.view.cursor.filter(|_| ime.is_none()).map(|(x, y)| [x as f32 * self.grid.cell_w, y as f32 * self.grid.cell_h, self.grid.cell_w, self.grid.cell_h]);
        if let (Some(window), Some(r)) = (&self.window, ime.or(field)) {
            let rect = [r[0] as i32, r[1] as i32, r[2] as i32, r[3] as i32];
            if self.ime_rect != Some(rect) {
                window.set_ime_cursor_area(PhysicalPosition::new(rect[0], rect[1]), PhysicalSize::new(rect[2] as u32, rect[3] as u32));
                self.ime_rect = Some(rect);
            }
        }
    }

    fn build_scene(&mut self, width: u32, height: u32) -> (Scene, crate::tiles::CursorRect) {
        let mut scene = Scene::new(width, height, BG);
        let Some(fonts) = self.fonts.as_mut() else { return (scene, None) };
        let grid = self.grid;
        let mut p = Painter { scene: &mut scene, cache: &mut self.cache, fonts };
        if let Some(model) = &self.view.canvas {
            crate::canvas::paint(&mut p, &grid, model);
        }
        if let Some(modal) = self.view.modal {
            // A soft shadow lifts the top dialog or menu off what is under it.
            let (x, y, w, h) = (modal.x as f32 * grid.cell_w, modal.y as f32 * grid.cell_h, modal.width as f32 * grid.cell_w, modal.height as f32 * grid.cell_h);
            let full = [0.0, 0.0, width as f32, height as f32];
            for (spread, alpha) in [(14.0, 0.12), (7.0, 0.2), (3.0, 0.3)] {
                let s = spread * grid.scale;
                p.under(scene::OVERLAY, Quad::rounded(x - s + 3.0 * grid.scale, y - s + 5.0 * grid.scale, w + 2.0 * s, h + 2.0 * s, s + 4.0 * grid.scale, theme::rgba([0, 0, 0], alpha), [0.0; 4], 0.0, full));
            }
        }
        let inners: Vec<Rect> = self.view.tiles.iter().map(|t| t.inner).collect();
        let regions = crate::chrome::Regions { canvas: self.view.canvas.as_ref().map(|c| c.area), inners: &inners };
        crate::chrome::paint(&mut p, &grid, &self.chrome, Some(&self.overlay), &regions);
        let input = TileInput {
            selection: self.app.term_select.as_ref(),
            links: &self.app.layout.links,
            hover: self.hover,
            ctrl: self.mods.control_key(),
            preedit: self.preedit.as_deref(),
        };
        let ime = crate::tiles::paint(&mut p, &grid, &self.view.tiles, &self.app.views, &mut self.tile_cache, &input);
        // The text cursor of a field (terminals draw their own).
        if let Some((x, y)) = self.view.cursor.filter(|(x, y)| !inners.iter().any(|r| *x >= r.x && *x < r.right() && *y >= r.y && *y < r.bottom())) {
            let layer = if self.view.modal.is_some() { scene::OVERLAY } else { scene::CHROME_TOP };
            let thin = (2.0 * grid.scale).round().max(2.0);
            let full = [0.0, 0.0, width as f32, height as f32];
            p.over(layer, Quad::rect((x as f32 * grid.cell_w).round(), (y as f32 * grid.cell_h).round(), thin, grid.cell_h, theme::color(canvas_core::paint::TEXT, 1.0), full));
        }
        if let Some(text) = &self.view.banner {
            // Centred over the canvas (else the window).
            let center = self.view.canvas.as_ref().map(|c| (c.area.x as f32 + c.area.width as f32 / 2.0) * grid.cell_w).unwrap_or(width as f32 / 2.0);
            banner(&mut p, &grid, text, center, width);
        }
        if self.options.perf {
            let lines = self.perf.lines(self.app.conn.remote);
            hud(&mut p, &grid, &lines, width, self.rows);
        }
        (scene, ime)
    }

    /// Renders the current frame offscreen to a PNG.
    fn png(&mut self, path: &std::path::Path) -> anyhow::Result<(u32, u32)> {
        if self.app.needs_draw || self.cols == 0 {
            self.frame();
        }
        let (width, height) = self.grid_size();
        let (scene, _) = self.build_scene(width, height);
        let (Some(gpu), Some(fonts)) = (&mut self.gpu, &mut self.fonts) else { anyhow::bail!("no GPU device") };
        let rgba = gpu.capture(&scene, &self.cache, fonts)?;
        crate::gpu::write_png(path, width, height, &rgba)?;
        Ok((width, height))
    }

    fn on_hook(&mut self, event_loop: &ActiveEventLoop, command: &Value) -> Value {
        let name = command["cmd"].as_str().unwrap_or_default();
        let mods = parse_mods(&command["mods"]);
        let point = || (command["x"].as_f64().unwrap_or(0.0) as f32, command["y"].as_f64().unwrap_or(0.0) as f32);
        match name {
            "key" => match hook_key(command, mods) {
                Some(key) => {
                    if !self.gui_key(&key) {
                        self.input(vec![InputEvent::Key(key)]);
                    }
                    json!({ "ok": true })
                }
                None => json!({ "ok": false, "error": "unknown key" }),
            },
            "keys" => {
                // A batch, as one read of a console's input.
                let mut batch = Vec::new();
                for spec in command["keys"].as_array().into_iter().flatten() {
                    let Some(key) = hook_key(spec, parse_mods(&spec["mods"])) else { return json!({ "ok": false, "error": format!("unknown key {spec}") }) };
                    if is_paste_key(&key) {
                        if !batch.is_empty() {
                            self.input(std::mem::take(&mut batch));
                        }
                        if self.gui_key(&key) {
                            continue;
                        }
                    }
                    batch.push(InputEvent::Key(key));
                }
                if !batch.is_empty() {
                    self.input(batch);
                }
                json!({ "ok": true })
            }
            "ime" => {
                // The events an IME sends: composition text, then the commit.
                match (command["preedit"].as_str(), command["commit"].as_str()) {
                    (Some(text), _) => {
                        self.preedit = (!text.is_empty()).then(|| text.to_owned());
                        self.redraw = true;
                    }
                    (_, Some(text)) => {
                        self.preedit = None;
                        self.input(vec![InputEvent::Text(text.to_owned())]);
                    }
                    _ => {}
                }
                json!({ "ok": true })
            }
            "text" => {
                self.input(vec![InputEvent::Text(command["text"].as_str().unwrap_or_default().to_owned())]);
                json!({ "ok": true })
            }
            "paste" => {
                self.input(vec![InputEvent::Paste(command["text"].as_str().unwrap_or_default().to_owned())]);
                json!({ "ok": true })
            }
            "mouse" => {
                let button = match command["button"].as_str().unwrap_or("left") {
                    "right" => MouseButton::Right,
                    "middle" => MouseButton::Middle,
                    _ => MouseButton::Left,
                };
                let kind = match command["kind"].as_str().unwrap_or_default() {
                    "down" => MouseEventKind::Down(button),
                    "up" => MouseEventKind::Up(button),
                    "drag" => MouseEventKind::Drag(button),
                    _ => MouseEventKind::Moved,
                };
                self.input(vec![InputEvent::Mouse(canvas_core::input::MouseEvent::at(kind, point().0, point().1, mods))]);
                json!({ "ok": true })
            }
            "wheel" => {
                let kind = match command["dir"].as_str().unwrap_or("down") {
                    "up" => MouseEventKind::ScrollUp,
                    "left" => MouseEventKind::ScrollLeft,
                    "right" => MouseEventKind::ScrollRight,
                    _ => MouseEventKind::ScrollDown,
                };
                self.input(vec![InputEvent::Mouse(canvas_core::input::MouseEvent::at(kind, point().0, point().1, mods))]);
                json!({ "ok": true })
            }
            "screen" => {
                if self.app.needs_draw || self.redraw {
                    self.frame();
                }
                json!({ "ok": true, "cols": self.cols, "rows": self.rows, "lines": crate::hooks::screen(&self.app, &self.view, &self.chrome, &self.overlay) })
            }
            "tree" => {
                if self.app.needs_draw || self.redraw {
                    self.frame();
                }
                json!({ "ok": true, "tree": crate::hooks::tree(&self.app, &self.view, &self.chrome) })
            }
            "png" => {
                let path = std::path::PathBuf::from(command["path"].as_str().unwrap_or("gui.png"));
                match self.png(&path) {
                    Ok((w, h)) => json!({ "ok": true, "path": path, "width": w, "height": h }),
                    Err(error) => json!({ "ok": false, "error": format!("{error:#}") }),
                }
            }
            "size" => json!({
                "ok": true, "cols": self.cols, "rows": self.rows, "cell_w": self.grid.cell_w, "cell_h": self.grid.cell_h, "scale": self.grid.scale,
                "adapter": self.gpu.as_ref().map(|g| g.info.name.clone()), "present_mode": self.surface.as_ref().map(|s| format!("{:?}", s.present_mode())),
            }),
            "perf" => {
                if command["reset"].as_bool() == Some(true) {
                    self.perf.reset();
                }
                let (cpu, private) = crate::perf::process();
                json!({
                    "ok": true,
                    "perf": self.perf.json(),
                    "cpu_ms": cpu.as_millis() as u64,
                    "private_mb": private as f64 / (1024.0 * 1024.0),
                    "first_paint_ms": self.first_paint.map(|d| d.as_millis() as u64),
                    "shape_cache": self.cache.len(),
                    "shaped": self.cache.shaped,
                    "rows_rebuilt": self.tile_cache.rebuilt,
                    "boxes": self.view.canvas.as_ref().map(|c| c.boxes.len()),
                    "connected": self.app.connected,
                })
            }
            "redraw" => {
                // Forces a full frame (measurements).
                self.app.needs_draw = true;
                self.frame();
                json!({ "ok": true })
            }
            "post_keys" => {
                // Real window messages, through winit's keyboard handling.
                let hwnd = self.window.as_ref().map(|w| hwnd_of(w)).unwrap_or(0);
                let text = command["text"].as_str().unwrap_or_default().to_owned();
                let every = Duration::from_millis(command["interval_ms"].as_u64().unwrap_or(100));
                std::thread::spawn(move || {
                    for c in text.chars() {
                        post_key(hwnd, c);
                        std::thread::sleep(every);
                    }
                });
                json!({ "ok": true })
            }
            "quit" => {
                self.app.quit = true;
                self.finish(event_loop);
                json!({ "ok": true })
            }
            other => json!({ "ok": false, "error": format!("unknown command {other:?}") }),
        }
    }

    fn create_window(&mut self, event_loop: &ActiveEventLoop) -> anyhow::Result<()> {
        let fonts = match self.font_loader.take() {
            Some(loader) => loader.join().map_err(|_| anyhow::anyhow!("font loading failed"))?,
            None => crate::text::load(),
        };
        trace("fonts");
        let monitor_scale = event_loop.primary_monitor().map(|m| m.scale_factor()).unwrap_or(1.0) as f32;
        let grid = Grid::new(fonts.advance, monitor_scale);
        let mut attrs = Window::default_attributes().with_title("Copilot Canvas").with_min_inner_size(LogicalSize::new(640.0, 360.0));
        attrs = match self.options.size {
            Some((cols, rows)) => attrs.with_inner_size(PhysicalSize::new((cols as f32 * grid.cell_w) as u32, (rows as f32 * grid.cell_h) as u32)),
            None => attrs.with_inner_size(LogicalSize::new(1560.0, 940.0)),
        };
        let window = Arc::new(event_loop.create_window(attrs)?);
        window.set_ime_allowed(true);
        self.platform.set_window(hwnd_of(&window));
        self.grid = Grid::new(fonts.advance, window.scale_factor() as f32);
        self.fonts = Some(fonts);
        if let Some(hz) = window.current_monitor().and_then(|m| m.refresh_rate_millihertz()).filter(|m| *m > 0) {
            self.frame_interval = Duration::from_secs_f64(1000.0 / hz as f64);
        }
        trace("window");
        let (gpu, surface) = Gpu::new(window.clone(), event_loop.owned_display_handle())?;
        self.gpu = Some(gpu);
        self.surface = Some(surface);
        trace("gpu");
        let size = window.inner_size();
        self.window = Some(window);
        self.fit(size.width, size.height);
        Ok(())
    }

    /// The mouse pointer's shape: a hand over buttons and links, resize
    /// arrows over the dividers, a text beam over terminals.
    fn update_cursor(&mut self, (x, y): (u16, u16)) {
        use winit::window::CursorIcon;
        let layout = &self.app.layout;
        let inside = |r: &Rect| x >= r.x && x < r.right() && y >= r.y && y < r.bottom();
        let icon = if layout.links.iter().any(|l| l.contains(x, y)) || layout.hits.iter().any(|(r, _)| inside(r)) {
            CursorIcon::Pointer
        } else if self.app.modals.is_empty() && (Some(x) == layout.sidebar_divider || Some(x) == layout.term_divider) && y > 0 && y + 1 < self.rows {
            CursorIcon::ColResize
        } else if self.app.modals.is_empty() && layout.tiles.iter().any(|(_, _, inner)| inside(inner)) {
            CursorIcon::Text
        } else {
            CursorIcon::Default
        };
        if self.cursor_icon != Some(icon) {
            if let Some(window) = &self.window {
                window.set_cursor(icon);
            }
            self.cursor_icon = Some(icon);
        }
    }

    /// Saves and leaves once the app quits (detach or stop all).
    fn finish(&mut self, event_loop: &ActiveEventLoop) {
        if !self.app.quit || self.exiting {
            return;
        }
        self.exiting = true;
        self.app.save_now();
        if let Some(error) = &self.app.save_error {
            eprintln!("copilot-canvas-gui: the canvas could not be saved: {error}");
            self.exit_code = 1;
        }
        event_loop.exit();
    }

    fn mouse(&mut self, kind: MouseEventKind) {
        let event = crate::input::mouse(kind, self.pointer, self.mods);
        self.input(vec![event]);
    }
}

/// CANVAS_GUI_TRACE=1: start-up milestones on stderr (ms since process start).
fn trace(what: &str) {
    if std::env::var_os("CANVAS_GUI_TRACE").is_some() {
        eprintln!("{:>6} ms  {what}", crate::perf::since_start().as_millis());
    }
}

fn hwnd_of(window: &Window) -> isize {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
        _ => 0,
    }
}

/// Posts a key press for an ASCII character to the window (as the OS would).
fn post_key(hwnd: isize, c: char) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{MAPVK_VK_TO_VSC, MapVirtualKeyW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_KEYDOWN, WM_KEYUP};
    let vk = match c {
        '\r' | '\n' => 0x0d,
        '\u{8}' => 0x08,
        c => c.to_ascii_uppercase() as u32,
    };
    // SAFETY: posting messages to this process's own window.
    unsafe {
        let scan = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC);
        let down = 1isize | ((scan as isize) << 16);
        PostMessageW(hwnd as _, WM_KEYDOWN, vk as usize, down);
        std::thread::sleep(Duration::from_millis(10));
        PostMessageW(hwnd as _, WM_KEYUP, vk as usize, down | (1 << 30) | (1 << 31));
    }
}

/// Ctrl+V or Shift+Insert.
fn is_paste_key(key: &KeyEvent) -> bool {
    let ctrl_v = key.modifiers.contains(Modifiers::CONTROL) && matches!(key.code, KeyCode::Char('v' | 'V'));
    ctrl_v || key.modifiers.contains(Modifiers::SHIFT) && key.code == KeyCode::Insert
}

fn parse_mods(value: &Value) -> Modifiers {
    let mut m = Modifiers::NONE;
    for name in value.as_array().into_iter().flatten().filter_map(Value::as_str) {
        match name {
            "shift" => m |= Modifiers::SHIFT,
            "ctrl" => m |= Modifiers::CONTROL,
            "alt" => m |= Modifiers::ALT,
            "super" => m |= Modifiers::SUPER,
            _ => {}
        }
    }
    m
}

/// `{"cmd":"key","key":"Enter"}` or `{"cmd":"key","char":"a","mods":["ctrl"]}`.
fn hook_key(command: &Value, mods: Modifiers) -> Option<KeyEvent> {
    if let Some(c) = command["char"].as_str().and_then(|s| s.chars().next()) {
        return Some(KeyEvent::new(KeyCode::Char(c), mods));
    }
    let code = match command["key"].as_str()? {
        "Enter" => KeyCode::Enter,
        "Tab" => KeyCode::Tab,
        "BackTab" => KeyCode::BackTab,
        "Backspace" => KeyCode::Backspace,
        "Esc" => KeyCode::Esc,
        "Up" => KeyCode::Up,
        "Down" => KeyCode::Down,
        "Left" => KeyCode::Left,
        "Right" => KeyCode::Right,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PageUp" => KeyCode::PageUp,
        "PageDown" => KeyCode::PageDown,
        "Insert" => KeyCode::Insert,
        "Delete" => KeyCode::Delete,
        f if f.starts_with('F') => KeyCode::F(f[1..].parse().ok()?),
        _ => return None,
    };
    Some(KeyEvent::new(code, mods))
}

/// The connection banner, prominent at the top.
fn banner(p: &mut Painter, grid: &Grid, text: &str, center: f32, width: u32) {
    let size = grid.font_px;
    let (key, w) = p.shape(Spec::ui(&format!("●  {text}"), size).bold(true));
    let (bw, bh) = (w + 2.0 * grid.cell_w, grid.cell_h * 1.4);
    let x = (center - bw / 2.0).clamp(0.0, (width as f32 - bw).max(0.0));
    let y = grid.cell_h * 1.6;
    let full = [0.0, 0.0, width as f32, 1e6];
    p.under(scene::HUD, Quad::rounded(x, y, bw, bh, bh / 2.0, theme::color(canvas_core::paint::NEGATIVE, 0.95), [0.0; 4], 0.0, full));
    p.place(scene::HUD, key, x + grid.cell_w, y + (bh - size * 1.3) / 2.0, 1.0, [255, 255, 255], full);
}

/// The `--perf` overlay at the bottom right, above the status bar.
fn hud(p: &mut Painter, grid: &Grid, lines: &[String], width: u32, rows: u16) {
    let size = grid.font_px * 0.85;
    let line_h = size * 1.3;
    let widths: Vec<(u64, f32)> = lines.iter().map(|l| p.shape(Spec::mono(l, size))).collect();
    let w = widths.iter().map(|(_, w)| *w).fold(0.0, f32::max) + grid.cell_w * 2.0;
    let h = line_h * lines.len() as f32 + grid.cell_h * 0.6;
    let x = width as f32 - w - grid.cell_w;
    let y = (rows.saturating_sub(1) as f32 * grid.cell_h - h - grid.cell_h * 0.4).max(grid.cell_h);
    let full = [0.0, 0.0, width as f32, 1e6];
    p.under(scene::HUD, Quad::rounded(x, y, w, h, 6.0 * grid.scale, theme::rgba([6, 8, 12], 0.94), theme::color(canvas_core::paint::FAINT, 1.0), grid.scale, full));
    for (i, (key, _)) in widths.into_iter().enumerate() {
        p.place(scene::HUD, key, x + grid.cell_w, y + grid.cell_h * 0.3 + i as f32 * line_h, 1.0, theme::rgb(canvas_core::paint::TEXT, theme::FG), full);
    }
}

impl ApplicationHandler<UserEvent> for Shell {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        trace("resumed");
        if let Err(error) = self.create_window(event_loop) {
            eprintln!("copilot-canvas-gui: {error:#}");
            self.exit_code = 1;
            event_loop.exit();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Service => self.drain_service(),
            UserEvent::ServiceClosed(generation) => {
                if generation == self.svc_gen && self.app.restarting.is_none() {
                    // Reconnects in the background; input is blocked meanwhile.
                    self.app.connection_lost();
                }
            }
            UserEvent::App(event) => {
                self.app.on_app_event(*event);
                self.after_events();
            }
            UserEvent::Hook(request) => {
                trace("hook");
                let reply = self.on_hook(event_loop, &request.command);
                let _ = request.reply.send(reply);
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if self.window.as_ref().is_some_and(|w| w.id() != id) {
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                // As Ctrl+Q: asks to detach or stop the running sessions.
                self.app.request_quit();
                self.input_waiting = true;
            }
            WindowEvent::Resized(size) => {
                if let (Some(gpu), Some(surface)) = (&self.gpu, &mut self.surface) {
                    surface.resize(gpu, size.width, size.height);
                }
                self.fit(size.width, size.height);
                self.redraw = true;
                self.retry_at = None;
                self.retries = 0;
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(fonts) = &self.fonts {
                    self.grid = Grid::new(fonts.advance, scale_factor as f32);
                }
                self.redraw = true;
            }
            WindowEvent::RedrawRequested => self.redraw = true,
            WindowEvent::Focused(focused) => self.input(vec![InputEvent::Focus(focused)]),
            WindowEvent::ModifiersChanged(m) => {
                let ctrl = self.mods.control_key() != m.state().control_key();
                self.mods = m.state();
                if ctrl && self.app.layout.links.iter().any(|l| l.terminal) {
                    self.redraw = true;
                }
            }
            WindowEvent::KeyboardInput { event, is_synthetic, .. } => {
                if is_synthetic || self.preedit.is_some() {
                    return;
                }
                if matches!(event.logical_key, Key::Named(NamedKey::Control | NamedKey::Shift | NamedKey::Alt | NamedKey::Super)) {
                    return;
                }
                let Some(input) = crate::input::key(&event.logical_key, event.state, self.mods) else { return };
                if let InputEvent::Key(key) = &input
                    && self.gui_key(key)
                {
                    return;
                }
                self.input(vec![input]);
            }
            WindowEvent::Ime(ime) => match ime {
                Ime::Preedit(text, _) => {
                    self.preedit = (!text.is_empty()).then_some(text);
                    self.redraw = true;
                }
                Ime::Commit(text) => {
                    self.preedit = None;
                    self.input(vec![InputEvent::Text(text)]);
                }
                Ime::Enabled | Ime::Disabled => {}
            },
            WindowEvent::CursorMoved { position, .. } => {
                let point = (position.x as f32 / self.grid.cell_w, position.y as f32 / self.grid.cell_h);
                let cell = (point.0.max(0.0) as u16, point.1.max(0.0) as u16);
                let moved_cell = (self.pointer.0 as u16, self.pointer.1 as u16) != cell;
                self.pointer = point;
                if let Some(&button) = self.held.last() {
                    self.mouse(MouseEventKind::Drag(button));
                } else if moved_cell {
                    // Hover: link underlines follow the pointer.
                    let over_link = |c: Option<(u16, u16)>| c.is_some_and(|(x, y)| self.app.layout.links.iter().any(|l| l.terminal && l.contains(x, y)));
                    if over_link(self.hover) || over_link(Some(cell)) {
                        self.redraw = true;
                    }
                    self.hover = Some(cell);
                    self.update_cursor(cell);
                }
            }
            WindowEvent::CursorLeft { .. } => self.hover = None,
            WindowEvent::MouseInput { state, button, .. } => {
                let Some(button) = crate::input::button(button) else { return };
                match state {
                    ElementState::Pressed => {
                        self.held.retain(|b| *b != button);
                        self.held.push(button);
                        self.mouse(MouseEventKind::Down(button));
                    }
                    ElementState::Released => {
                        self.held.retain(|b| *b != button);
                        self.mouse(MouseEventKind::Up(button));
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                for kind in self.wheel.notches(delta, self.grid.cell_h) {
                    self.mouse(kind);
                }
            }
            _ => {}
        }
        self.finish(event_loop);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            return;
        }
        let now = Instant::now();
        if now >= self.next_tick {
            let stats_at = self.app.stats_at;
            self.app.on_tick();
            if self.app.stats_at != stats_at {
                self.stats_sent = Some(self.app.stats_at);
            }
            let app = &self.app;
            let busy = app.needs_draw || app.notice.is_some() || app.flash.is_some() || !app.pulses.is_empty() || app.dirty_at.is_some() || app.drag.is_some() || app.reconnecting || app.restarting.is_some();
            self.next_tick = now + if busy { TICK } else { IDLE_TICK };
            self.after_events();
        }
        if let Some(window) = &self.window {
            self.platform.apply(window, &mut self.taskbar);
        }
        if self.app.quit {
            self.finish(event_loop);
            return;
        }
        let due = |s: &Self| -> Option<Instant> {
            if !(s.app.needs_draw || s.redraw) {
                return None;
            }
            if let Some(at) = s.retry_at {
                return Some(at);
            }
            Some(if s.input_waiting { Instant::now() } else { s.last_draw + s.frame_interval })
        };
        if due(self).is_some_and(|at| at <= Instant::now()) {
            self.frame();
        }
        let wake = [Some(self.next_tick), due(self)].into_iter().flatten().min().unwrap_or(self.next_tick);
        event_loop.set_control_flow(ControlFlow::WaitUntil(wake));
    }
}
