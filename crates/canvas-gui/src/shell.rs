//! The windows and their event loop. Every window runs its own canvas-core
//! `App` (its own service connection and per-client preferences; the
//! service's document sync keeps them consistent) and shares the GPU device
//! and the fonts. Winit events become core input, service messages and app
//! events wake the loop through the proxy, frames are drawn only when
//! something changed (input immediately, background updates paced to the
//! display), and ticks run every 40 ms while anything is active. The process
//! ends when its last window closes.
//!
//! A frame in which only terminal contents changed (streaming output) keeps
//! the previous frame's other layers: it repaints the terminal tiles and the
//! HUD only. A lost GPU device is rebuilt with every window's surface; a DPI
//! change re-lays out the cells and keeps the canvas centred; each window's
//! placement is saved and restored; AccessKit exposes the UI Automation tree.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

use crate::gpu::{self, Gpu, Skip, Surface};
use crate::perf::{Perf, ms};
use crate::placement::Placement;
use crate::os::Taskbar;
use crate::platform::GuiPlatform;
use crate::quads::Quad;
use crate::scene::{self, ImageItem, Painter, Scene};
use crate::text::{Fonts, Grid, ShapeCache, Spec};
use crate::theme::{self, BG};
use crate::tiles::{TileCache, TileInput};

/// Housekeeping interval while anything is active (as the TUI).
const TICK: Duration = Duration::from_millis(40);
/// … and while nothing is.
const IDLE_TICK: Duration = Duration::from_millis(400);
/// Housekeeping while only effects animate (their frames are 80 ms apart).
const EFFECT_TICK: Duration = Duration::from_millis(80);
/// Moving loop dots are redrawn at most this often (30 frames/s, the loop
/// layer alone; small arcs less often, see `canvas::loop_interval`).
const LOOP_FRAME: Duration = Duration::from_millis(33);
/// Longest the UI thread spends on service messages before handling input again.
const SERVICE_BUDGET: Duration = Duration::from_millis(4);
/// How long the preview of a pasted image stays.
const PREVIEW_FOR: Duration = Duration::from_secs(5);
/// Placement changes are written this long after the last one.
const PLACEMENT_SAVE: Duration = Duration::from_secs(1);
/// The accessibility tree is sent at most this often.
const ACCESS_EVERY: Duration = Duration::from_millis(150);

pub enum UserEvent {
    /// Service messages are waiting for window `.0`.
    Service(usize),
    /// The service connection of window `.0` and generation `.1` closed.
    ServiceClosed(usize, u64),
    App(usize, Box<AppEvent>),
    Hook(crate::hooks::Request),
    /// Another window's app is ready (or could not be opened); `reply`
    /// answers the test hook that asked for it.
    Opened(Box<Result<crate::Opened, String>>, Option<Sender<Value>>),
    /// The GPU device may be lost (its callback or an error said so).
    GpuCheck,
    /// A screen reader or UI Automation client asked something.
    Access(accesskit_winit::Event),
    /// The HTML view of a report has news (ready, failed, a clicked link, Esc).
    Web,
}

impl From<accesskit_winit::Event> for UserEvent {
    fn from(event: accesskit_winit::Event) -> Self {
        UserEvent::Access(event)
    }
}

/// Wakes the event loop to check the GPU device (from any thread).
pub fn wake() {
    if let Some(proxy) = PROXY.get() {
        let _ = proxy.send_event(UserEvent::GpuCheck);
    }
}

/// Wakes the event loop for the HTML views' news.
pub fn wake_web() {
    if let Some(proxy) = PROXY.get() {
        let _ = proxy.send_event(UserEvent::Web);
    }
}

pub struct Options {
    pub perf: bool,
    pub test_hooks: bool,
    /// Initial window size in cells (fixed with test hooks).
    pub size: Option<(u16, u16)>,
    /// The data folder and instance (window placements are saved there).
    pub home: std::path::PathBuf,
    pub instance: String,
}

/// Opens the app of window `index` (connection, document, preferences).
pub type Opener = Arc<dyn Fn(usize) -> anyhow::Result<crate::Opened> + Send + Sync>;

/// The image shown after pasting one into a terminal.
struct Preview {
    item: ImageItem,
    /// The clipboard image's size.
    size: (u32, u32),
    until: Instant,
}

/// One window: its app, swap chain, layout and input state.
struct Win {
    index: usize,
    app: App,
    platform: Arc<GuiPlatform>,
    taskbar: Taskbar,
    window: Arc<Window>,
    /// `None` while the GPU objects are rebuilt.
    surface: Option<Surface>,
    grid: Grid,
    cols: u16,
    rows: u16,
    cache: ShapeCache,
    tile_cache: TileCache,
    chrome: Buffer,
    overlay: Buffer,
    view: View,
    /// The last frame's scene (its layers are still on the GPU).
    scene: Option<Scene>,
    /// Terminals whose contents changed since the last frame while nothing
    /// else did: the next frame repaints only the tiles.
    damaged: Vec<String>,
    damage_frames: u64,
    /// Scratch cells for finding links in damaged tiles.
    scratch: Buffer,
    /// After a DPI change: the world point kept at the canvas's centre, until when.
    recenter: Option<((f32, f32), Instant)>,
    placement: Placement,
    placement_path: std::path::PathBuf,
    placement_dirty: Option<Instant>,
    access: Option<accesskit_winit::Adapter>,
    /// An accessibility client is listening.
    access_active: bool,
    access_due: bool,
    access_at: Option<Instant>,
    /// The last nodes sent (actions find their targets here).
    access_items: Vec<crate::access::Item>,
    /// The last present found the device gone.
    device_lost: bool,
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
    /// When the loop layer was last drawn, and the clock it was drawn at.
    loop_drawn: Instant,
    scene_clock: u64,
    /// How often the loop layer is redrawn while its dots move (from the arcs' size).
    loop_interval: Duration,
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
    first_paint: Option<Duration>,
    /// Files dropped since the last pass of the loop.
    dropped: Vec<String>,
    preview: Option<Preview>,
    focused: bool,
    /// The HTML view of an open HTML report (WebView2, created when first needed).
    web: crate::os::webview::ReportWeb,
}

pub struct Shell {
    options: Options,
    proxy: EventLoopProxy<UserEvent>,
    opener: Opener,
    font_loader: Option<std::thread::JoinHandle<Fonts>>,
    fonts: Option<Fonts>,
    gpu: Option<Gpu>,
    wins: Vec<Win>,
    /// Apps waiting for their window (the first waits for `resumed`).
    waiting: Vec<(crate::Opened, Option<Sender<Value>>)>,
    next_index: usize,
    /// GPU objects rebuilt after a lost device, and the next retry when that failed.
    recoveries: u64,
    recover_at: Option<Instant>,
    pub first_paint: Option<Duration>,
    pub exit_code: i32,
}

impl Shell {
    pub fn new(first: crate::Opened, options: Options, proxy: EventLoopProxy<UserEvent>, font_loader: std::thread::JoinHandle<Fonts>, opener: Opener) -> Self {
        Self {
            options,
            proxy,
            opener,
            font_loader: Some(font_loader),
            fonts: None,
            gpu: None,
            wins: Vec::new(),
            waiting: vec![(first, None)],
            next_index: 1,
            recoveries: 0,
            recover_at: None,
            first_paint: None,
            exit_code: 0,
        }
    }

    /// Opens another window on a background thread (connecting takes a moment).
    fn open_window(&mut self, reply: Option<Sender<Value>>) {
        let index = self.next_index;
        self.next_index += 1;
        let (opener, proxy) = (self.opener.clone(), self.proxy.clone());
        let _ = std::thread::Builder::new().name("open-window".into()).spawn(move || {
            let result = opener(index).map_err(|e| format!("{e:#}"));
            let _ = proxy.send_event(UserEvent::Opened(Box::new(result), reply));
        });
    }

    /// Creates the window for an opened app (the first one also opens the GPU
    /// device), where it was last time.
    fn create_window(&mut self, event_loop: &ActiveEventLoop, opened: crate::Opened) -> anyhow::Result<usize> {
        if self.fonts.is_none() {
            let fonts = match self.font_loader.take() {
                Some(loader) => loader.join().map_err(|_| anyhow::anyhow!("font loading failed"))?,
                None => crate::text::load(),
            };
            self.fonts = Some(fonts);
            trace("fonts");
        }
        let advance = self.fonts.as_ref().map(|f| f.advance).unwrap_or(0.6);
        let monitor_scale = event_loop.primary_monitor().map(|m| m.scale_factor()).unwrap_or(1.0) as f32;
        let grid = Grid::new(advance, monitor_scale);
        let index = opened.index;
        let client = if index == 0 { self.options.instance.clone() } else { format!("{}-window{}", self.options.instance, index + 1) };
        let placement_path = crate::placement::path(&self.options.home, &client);
        let saved = crate::placement::load(&placement_path).map(|p| crate::placement::clamp(p, &crate::placement::monitors(event_loop)));
        // Invisible until the accessibility adapter is attached.
        let mut attrs = Window::default_attributes()
            .with_title(if index == 0 { "Copilot Canvas".to_owned() } else { format!("Copilot Canvas · window {}", index + 1) })
            .with_min_inner_size(LogicalSize::new(640.0, 360.0))
            .with_visible(false);
        attrs = match (self.options.size, &saved) {
            (Some((cols, rows)), _) => attrs.with_inner_size(PhysicalSize::new((cols as f32 * grid.cell_w) as u32, (rows as f32 * grid.cell_h) as u32)),
            (None, Some(p)) => attrs.with_inner_size(PhysicalSize::new(p.width.max(320), p.height.max(200))),
            (None, None) => attrs.with_inner_size(LogicalSize::new(1560.0, 940.0)),
        };
        if let Some(p) = &saved {
            attrs = attrs.with_position(PhysicalPosition::new(p.x, p.y));
        }
        // The app icon embedded by build.rs: small in the title bar, large on
        // the taskbar button and in Alt+Tab.
        attrs = crate::os::with_app_icon(attrs);
        if self.options.test_hooks && quiet_tests() {
            // A test run on someone's desktop: never take the focus, stay out
            // of the taskbar and behind other windows (input comes from the hooks).
            attrs = crate::os::quiet_window(attrs);
        }
        let window = Arc::new(event_loop.create_window(attrs)?);
        let access = accesskit_winit::Adapter::with_event_loop_proxy(event_loop, &window, self.proxy.clone());
        window.set_visible(true);
        if saved.as_ref().is_some_and(|p| p.maximized) {
            window.set_maximized(true);
        }
        window.set_ime_allowed(true);
        opened.platform.set_window(hwnd_of(&window));
        let grid = Grid::new(advance, window.scale_factor() as f32);
        trace("window");
        let surface = match &self.gpu {
            Some(gpu) => gpu.surface(window.clone())?,
            None => {
                let (gpu, surface) = Gpu::new(window.clone(), event_loop.owned_display_handle())?;
                crate::crashlog::set_adapter(gpu.describe());
                self.gpu = Some(gpu);
                surface
            }
        };
        trace("gpu");
        let mut win = Win::new(opened, window.clone(), surface, grid, &self.proxy);
        win.access = Some(access);
        win.web = crate::os::webview::ReportWeb::new(&self.options.home);
        win.placement_path = placement_path;
        win.placement = saved.unwrap_or_default();
        win.track_placement();
        win.placement_dirty = None;
        if self.gpu.as_ref().is_some_and(|g| g.software) {
            win.app.notify("No usable GPU: drawing with Windows' software renderer (WARP), which costs more CPU.", false);
        }
        if let Some(hz) = window.current_monitor().and_then(|m| m.refresh_rate_millihertz()).filter(|m| *m > 0) {
            win.frame_interval = Duration::from_secs_f64(1000.0 / hz as f64);
        }
        let size = window.inner_size();
        win.fit(size.width, size.height, &self.options);
        self.wins.push(win);
        crate::crashlog::set_windows(self.wins.len());
        Ok(index)
    }

    /// Builds the GPU device, pipelines, atlas and every window's surface
    /// again (a lost device, or the `device-lost` hook); image textures are
    /// uploaded again as they are drawn.
    fn recover_gpu(&mut self, event_loop: &ActiveEventLoop, reason: &str) -> anyhow::Result<Duration> {
        let started = Instant::now();
        crate::crashlog::event("gpu", &format!("rebuilding the GPU objects: {reason}"));
        for win in &mut self.wins {
            win.surface = None;
            win.scene = None;
        }
        drop(self.gpu.take());
        let first = self.wins.first().map(|w| w.window.clone()).ok_or_else(|| anyhow::anyhow!("no window"))?;
        let result = (|| -> anyhow::Result<()> {
            let (gpu, surface) = Gpu::new(first, event_loop.owned_display_handle())?;
            crate::crashlog::set_adapter(gpu.describe());
            self.wins[0].surface = Some(surface);
            for win in self.wins.iter_mut().skip(1) {
                win.surface = Some(gpu.surface(win.window.clone())?);
            }
            self.gpu = Some(gpu);
            Ok(())
        })();
        if let Err(error) = result {
            // Try again shortly (a driver being reset may need a moment).
            self.recover_at = Some(Instant::now() + Duration::from_secs(2));
            crate::crashlog::event("gpu", &format!("rebuilding failed: {error:#}"));
            return Err(error);
        }
        self.recover_at = None;
        self.recoveries += 1;
        for win in &mut self.wins {
            win.redraw = true;
            win.app.needs_draw = true;
            win.input_waiting = true;
            win.retry_at = None;
        }
        let took = started.elapsed();
        crate::crashlog::event("gpu", &format!("rebuilt in {} ms", took.as_millis()));
        Ok(took)
    }

    /// Rebuilds the GPU objects when the device is gone (or a retry is due).
    fn check_gpu(&mut self, event_loop: &ActiveEventLoop) {
        let lost = self.gpu.as_ref().is_some_and(|g| g.lost.load(Ordering::Acquire)) || self.wins.iter().any(|w| w.device_lost);
        let retry = self.recover_at.is_some_and(|t| Instant::now() >= t);
        if (lost || retry) && !self.wins.is_empty() {
            for win in &mut self.wins {
                win.device_lost = false;
            }
            let _ = self.recover_gpu(event_loop, if lost { "the device was lost" } else { "retry" });
        }
    }

    /// The window a hook command addresses (`"window": n`, else the first).
    fn target(&self, command: &Value) -> Option<usize> {
        match command["window"].as_u64() {
            Some(index) => self.wins.iter().position(|w| w.index == index as usize),
            None => (!self.wins.is_empty()).then_some(0),
        }
    }

    fn on_hook(&mut self, event_loop: &ActiveEventLoop, command: &Value, reply: Sender<Value>) {
        let name = command["cmd"].as_str().unwrap_or_default();
        crate::crashlog::set_hook(&command.to_string());
        match name {
            "new_window" => return self.open_window(Some(reply)),
            "device-lost" => {
                // As a driver reset: the device goes (`destroy`: really), then
                // the same recovery as for a reported loss.
                if command["destroy"].as_bool() == Some(true)
                    && let Some(gpu) = &self.gpu
                {
                    gpu.destroy();
                }
                if let Some(gpu) = &self.gpu {
                    gpu.lost.store(true, Ordering::Release);
                }
                let answer = match self.recover_gpu(event_loop, "test hook") {
                    Ok(took) => json!({ "ok": true, "ms": took.as_millis() as u64, "recoveries": self.recoveries, "adapter": self.gpu.as_ref().map(|g| g.describe()) }),
                    Err(error) => json!({ "ok": false, "error": format!("{error:#}") }),
                };
                let _ = reply.send(answer);
                return;
            }
            "windows" => {
                let list: Vec<Value> = self
                    .wins
                    .iter()
                    .map(|w| json!({ "window": w.index, "title": w.window.title(), "focused": w.focused, "cols": w.cols, "rows": w.rows, "connected": w.app.connected, "hwnd": hwnd_of(&w.window) }))
                    .collect();
                let _ = reply.send(json!({ "ok": true, "windows": list }));
                return;
            }
            "quit" if command.get("window").is_none() => {
                // Every window saves and detaches; the process ends with the last.
                for win in &mut self.wins {
                    win.app.quit = true;
                }
                let _ = reply.send(json!({ "ok": true }));
                self.close_finished(event_loop);
                return;
            }
            _ => {}
        }
        let Some(at) = self.target(command) else {
            let _ = reply.send(json!({ "ok": false, "error": "no such window" }));
            return;
        };
        let answer = match name {
            "webview" => {
                // The HTML view of reports: its state; `eval` (host script, the
                // page's own never runs), `capture` (a PNG of what it shows) and
                // `probe_raw` (unsanitized HTML: the view's own lock-down alone).
                let win = &mut self.wins[at];
                if let Some(script) = command["eval"].as_str() {
                    win.web.eval(script, reply);
                    return;
                }
                if let Some(path) = command["capture"].as_str() {
                    win.web.capture(std::path::PathBuf::from(path), reply);
                    return;
                }
                if let Some(html) = command["probe_raw"].as_str() {
                    win.web.probe_raw(html)
                } else if let Some(path) = command["window_capture"].as_str() {
                    // The window as composed, the HTML view over the canvas included.
                    let path = std::path::PathBuf::from(path);
                    match crate::os::webview::capture_window(hwnd_of(&win.window), &path) {
                        Ok((w, h)) => json!({ "ok": true, "path": path, "width": w, "height": h, "cell_w": win.grid.cell_w, "cell_h": win.grid.cell_h }),
                        Err(error) => json!({ "ok": false, "error": format!("{error:#}") }),
                    }
                } else {
                    let mut state = win.web.json();
                    state["pane"] = json!(win.view.html.as_ref().map(|h| json!({ "id": h.id, "rect": [h.rect.x, h.rect.y, h.rect.width, h.rect.height], "bytes": h.document.len() })));
                    state["core"] = json!(format!("{:?}", win.app.reports.web));
                    state
                }
            }
            "close_window" => {
                let others = self.wins.len() > 1;
                let win = &mut self.wins[at];
                if others { win.app.quit = true } else { win.app.request_quit() }
                win.input_waiting = true;
                json!({ "ok": true })
            }
            "png" => {
                let path = std::path::PathBuf::from(command["path"].as_str().unwrap_or("gui.png"));
                let (Some(gpu), Some(fonts)) = (&mut self.gpu, &mut self.fonts) else {
                    let _ = reply.send(json!({ "ok": false, "error": "no GPU device" }));
                    return;
                };
                match self.wins[at].png(gpu, fonts, &self.options, &path) {
                    // With the loop dots where this capture drew them.
                    Ok((w, h)) => json!({ "ok": true, "path": path, "width": w, "height": h, "loops": self.wins[at].loops_json(self.wins[at].scene_clock) }),
                    Err(error) => json!({ "ok": false, "error": format!("{error:#}") }),
                }
            }
            "perf" => {
                let textures = self.gpu.as_ref().map(|g| g.images.textures());
                let mut answer = self.wins[at].perf_json(command);
                answer["textures"] = json!(textures);
                answer["gpu_recoveries"] = json!(self.recoveries);
                answer
            }
            "dpi" => {
                let scale = command["scale"].as_f64().unwrap_or_else(|| self.wins[at].window.scale_factor()) as f32;
                let advance = self.fonts.as_ref().map(|f| f.advance).unwrap_or(0.6);
                let (gpu, options) = (self.gpu.as_ref(), &self.options);
                self.wins[at].rescale(scale, advance, gpu, options);
                let win = &self.wins[at];
                json!({ "ok": true, "scale": win.grid.scale, "cell_w": win.grid.cell_w, "cell_h": win.grid.cell_h, "cols": win.cols, "rows": win.rows })
            }
            "placement" => {
                let monitors: Vec<Value> = crate::placement::monitors(event_loop).into_iter().map(|m| json!({ "name": m.name, "work": m.work, "primary": m.primary })).collect();
                let win = &self.wins[at];
                let position = win.window.outer_position().ok().map(|p| [p.x, p.y]);
                let size = win.window.inner_size();
                json!({
                    "ok": true, "position": position, "size": [size.width, size.height], "maximized": win.window.is_maximized(),
                    "monitor": win.window.current_monitor().and_then(|m| m.name()), "tracked": win.placement, "saved": crate::placement::load(&win.placement_path),
                    "path": win.placement_path, "monitors": monitors,
                })
            }
            _ => {
                let (gpu, fonts) = (&mut self.gpu, &mut self.fonts);
                let win = &mut self.wins[at];
                match win.hook(command, &self.options, gpu.as_mut(), fonts.as_mut()) {
                    Some(answer) => answer,
                    None => json!({ "ok": false, "error": format!("unknown command {name:?}") }),
                }
            }
        };
        let _ = reply.send(answer);
        self.after_input(event_loop);
    }

    /// Opens windows the apps asked for and closes the ones that quit.
    fn after_input(&mut self, event_loop: &ActiveEventLoop) {
        let wanted = self.wins.iter_mut().map(|w| std::mem::take(&mut w.app.new_window_requested)).filter(|asked| *asked).count();
        for _ in 0..wanted {
            self.open_window(None);
        }
        self.close_finished(event_loop);
    }

    /// Saves and closes windows whose app quit; leaves when none is left.
    fn close_finished(&mut self, event_loop: &ActiveEventLoop) {
        let mut index = 0;
        while index < self.wins.len() {
            if self.wins[index].app.quit {
                let mut win = self.wins.remove(index);
                if let Some(error) = win.finish() {
                    eprintln!("copilot-canvas-gui: the canvas could not be saved: {error}");
                    self.exit_code = 1;
                }
            } else {
                index += 1;
            }
        }
        crate::crashlog::set_windows(self.wins.len());
        if self.wins.is_empty() && self.waiting.is_empty() && self.gpu.is_some() {
            event_loop.exit();
        }
    }

    fn by_window(&self, id: WindowId) -> Option<usize> {
        self.wins.iter().position(|w| w.window.id() == id)
    }

    fn by_index(&self, index: usize) -> Option<usize> {
        self.wins.iter().position(|w| w.index == index)
    }
}

impl Win {
    fn new(opened: crate::Opened, window: Arc<Window>, surface: Surface, grid: Grid, proxy: &EventLoopProxy<UserEvent>) -> Self {
        let crate::Opened { index, app, app_rx, platform } = opened;
        let (svc_tx, svc_rx) = unbounded();
        let area = Rect::new(0, 0, 1, 1);
        let mut win = Self {
            index,
            app,
            platform,
            taskbar: Taskbar::default(),
            window,
            surface: Some(surface),
            grid,
            cols: 0,
            rows: 0,
            cache: ShapeCache::default(),
            tile_cache: TileCache::default(),
            chrome: Buffer::empty(area),
            overlay: Buffer::empty(area),
            view: View::default(),
            scene: None,
            damaged: Vec::new(),
            damage_frames: 0,
            scratch: Buffer::empty(area),
            recenter: None,
            placement: Placement::default(),
            placement_path: std::path::PathBuf::new(),
            placement_dirty: None,
            access: None,
            access_active: false,
            access_due: false,
            access_at: None,
            access_items: Vec::new(),
            device_lost: false,
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
            loop_drawn: Instant::now() - Duration::from_secs(1),
            scene_clock: 0,
            loop_interval: LOOP_FRAME,
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
            dropped: Vec::new(),
            preview: None,
            focused: false,
            web: crate::os::webview::ReportWeb::new(std::path::Path::new(".")),
        };
        let rx = win.app.conn.rx.clone();
        win.forward_service(rx, proxy);
        let proxy = proxy.clone();
        std::thread::Builder::new()
            .name("app-events".into())
            .spawn(move || {
                for event in app_rx {
                    if proxy.send_event(UserEvent::App(index, Box::new(event))).is_err() {
                        break;
                    }
                }
            })
            .expect("app event thread");
        win
    }

    /// Forwards a service connection's messages to the UI thread (images are
    /// decoded from PNG on the way).
    fn forward_service(&mut self, rx: Receiver<ServiceMsg>, proxy: &EventLoopProxy<UserEvent>) {
        self.svc_gen += 1;
        let (generation, index) = (self.svc_gen, self.index);
        let (tx, pending, proxy) = (self.svc_tx.clone(), self.svc_pending.clone(), proxy.clone());
        let _ = std::thread::Builder::new().name("service-forward".into()).spawn(move || {
            for message in rx {
                let Some(message) = crate::images::decode_message(message) else { continue };
                if tx.send((generation, message)).is_err() {
                    return;
                }
                if !pending.swap(true, Ordering::AcqRel) && proxy.send_event(UserEvent::Service(index)).is_err() {
                    return;
                }
            }
            let _ = proxy.send_event(UserEvent::ServiceClosed(index, generation));
        });
    }

    fn drain_service(&mut self, proxy: &EventLoopProxy<UserEvent>) {
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
            // A frame that only changes a terminal's contents (not its scroll
            // position, not its first output) damages its tile, nothing else.
            let term = match &message {
                ServiceMsg::Frame8(frame) => Some(frame.id.clone()),
                ServiceMsg::Frame(frame) => Some(frame.id.clone()),
                _ => None,
            };
            let before = (self.app.needs_draw, term.as_ref().and_then(|id| self.app.views.get(id)).map(|v| v.display_offset));
            self.app.on_service(message);
            if let (Some(id), (false, Some(offset))) = (term, before)
                && self.app.views.get(&id).map(|v| v.display_offset) == Some(offset)
            {
                self.app.needs_draw = false;
                if self.view.tiles.iter().any(|t| t.id == id) && !self.damaged.contains(&id) {
                    self.damaged.push(id);
                }
            }
            if started.elapsed() >= SERVICE_BUDGET {
                // Leftovers after input and drawing.
                if !self.svc_pending.swap(true, Ordering::AcqRel) {
                    let _ = proxy.send_event(UserEvent::Service(self.index));
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

    fn after_events(&mut self, proxy: &EventLoopProxy<UserEvent>) {
        if let Some(rx) = self.app.new_service_rx.take() {
            self.forward_service(rx, proxy);
        }
    }

    /// Feeds input to the core (drawn at once, not paced).
    fn input(&mut self, events: Vec<InputEvent>, proxy: &EventLoopProxy<UserEvent>) {
        let now = Instant::now();
        self.perf.input_at.get_or_insert(now);
        let typing = self.typing();
        if typing && events.iter().any(|e| matches!(e, InputEvent::Key(_) | InputEvent::Text(_) | InputEvent::Paste(_))) {
            self.perf.key_at.get_or_insert(now);
            self.perf.echoed = false;
            self.typed_at = Some(now);
        }
        self.app.on_input(events);
        self.input_waiting = true;
        self.after_events(proxy);
    }

    /// Keyboard input goes to a terminal.
    fn typing(&self) -> bool {
        self.app.focus == Focus::Terminal && self.app.modals.is_empty()
    }

    /// Keys the window handles itself: Ctrl+Shift+N opens a window; Ctrl+V
    /// and Shift+Insert outside a terminal paste into the focused field (a
    /// terminal's own paste, with images, is the core's; pasting an image
    /// also shows what was attached).
    fn gui_key(&mut self, key: &KeyEvent, proxy: &EventLoopProxy<UserEvent>) -> bool {
        if key.modifiers.contains(Modifiers::CONTROL | Modifiers::SHIFT) && matches!(key.code, KeyCode::Char('n' | 'N')) {
            self.app.new_window_requested = true;
            return true;
        }
        let terminal = self.typing();
        if is_paste_key(key) && !terminal {
            if let Ok(text) = self.app.platform.clipboard_text() {
                self.input(vec![InputEvent::Paste(text)], proxy);
            }
            return true;
        }
        if terminal && (is_paste_key(key) || is_alt_v(key)) && self.app.platform.clipboard_has_image() {
            self.show_paste_preview();
        }
        false
    }

    /// Shows a thumbnail of the clipboard's image (pasted into a terminal).
    fn show_paste_preview(&mut self) {
        static SERIAL: AtomicU64 = AtomicU64::new(1);
        let Some((width, height, rgba)) = self.platform.test_image().or_else(crate::os::clip_image) else { return };
        let max = (self.grid.cell_w * 28.0) as u32;
        let (tw, th, pixels) = crate::images::thumbnail(width, height, &rgba, max, max * 2 / 3);
        let key = 1 << 63 | SERIAL.fetch_add(1, Ordering::Relaxed);
        let item = ImageItem { key, width: tw, height: th, rgba: pixels, rect: [0.0; 4], uv: [0.0, 0.0, 1.0, 1.0], clip: [0.0; 4] };
        self.preview = Some(Preview { item, size: (width, height), until: Instant::now() + PREVIEW_FOR });
        self.redraw = true;
    }

    fn surface_size(&self) -> (u32, u32) {
        let size = self.window.inner_size();
        (size.width.max(1), size.height.max(1))
    }

    fn grid_size(&self) -> (u32, u32) {
        ((self.cols as f32 * self.grid.cell_w) as u32, (self.rows as f32 * self.grid.cell_h) as u32)
    }

    /// Lays out the cells for a window size (unless the grid is fixed).
    fn fit(&mut self, width: u32, height: u32, options: &Options) {
        if width == 0 || height == 0 {
            return;
        }
        let (cols, rows) = match (options.test_hooks, options.size) {
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

    /// Builds the view, draws it and presents. When only terminal contents
    /// changed or loop dots moved, the previous frame's other layers stay (a
    /// damage frame: the tiles and HUD, and the loop layer).
    fn frame(&mut self, gpu: &mut Gpu, fonts: &mut Fonts, options: &Options) {
        if self.cols == 0 || self.surface.is_none() {
            return;
        }
        let started = Instant::now();
        let (width, height) = self.surface_size();
        let damage = !(self.app.needs_draw || self.redraw) && self.scene.as_ref().is_some_and(|s| (s.width, s.height) == (width, height));
        // Only loop dots moved: the loop layer alone (nothing else is rebuilt or rescanned).
        let mut loops_only = false;
        let (scene, ime, dirty, viewed) = if damage {
            let ids = std::mem::take(&mut self.damaged);
            let tiles = !ids.is_empty();
            let viewed = Instant::now();
            let mut scene = self.scene.take().unwrap_or_else(|| Scene::new(width, height, BG));
            self.damage_frames += 1;
            if self.damage_frames.is_multiple_of(64) {
                // The kept layers' shaped text stays in the cache.
                for layer in &scene.layers {
                    for t in &layer.texts {
                        self.cache.touch(t.key);
                    }
                }
            }
            let mut dirty = [false; scene::LAYERS];
            let mut ime = None;
            if tiles {
                // Links in the damaged terminals are found again; the rest of the layout stays.
                canvas_core::ui::rescan_tile_links(&mut self.app, &ids, &mut self.scratch);
                scene.layers[scene::TILES] = Default::default();
                scene.layers[scene::HUD] = Default::default();
                ime = self.paint_dynamic(&mut scene, fonts, options, width, height);
                dirty[scene::TILES] = true;
                dirty[scene::HUD] = true;
            }
            if let Some(model) = self.view.canvas.as_ref().filter(|m| m.loops_busy(m.anim_clock())) {
                scene.layers[scene::LOOPS] = Default::default();
                self.scene_clock = model.anim_clock();
                let mut p = Painter { scene: &mut scene, cache: &mut self.cache, fonts };
                crate::canvas::paint_loops(&mut p, &self.grid, model, self.scene_clock);
                dirty[scene::LOOPS] = true;
                self.loop_drawn = Instant::now();
                loops_only = !tiles;
            }
            (scene, ime, dirty, viewed)
        } else {
            self.damaged.clear();
            self.build_view();
            self.apply_recenter();
            let viewed = Instant::now();
            let (scene, ime) = self.build_scene(fonts, options, width, height);
            (scene, ime, gpu::ALL, viewed)
        };
        let built = Instant::now();
        // Minimized: nothing to present (the view is still built for the hooks).
        let minimized = self.window.inner_size().width == 0 || self.window.inner_size().height == 0;
        let surface = self.surface.as_mut().expect("surface");
        let result = (!minimized).then(|| gpu.present(surface, &self.window, &scene, &dirty, &self.cache, fonts));
        self.cache.end_frame();
        let now = Instant::now();
        self.last_draw = now;
        self.input_waiting = false;
        match result {
            Some(Ok(presented)) => {
                self.retries = 0;
                self.retry_at = None;
                let cost = ms(now - started - presented.acquire);
                if loops_only {
                    self.perf.loop_ms.push(cost);
                    self.perf.loop_frames += 1;
                } else if damage {
                    self.perf.damage_ms.push(cost);
                    self.perf.damage_frames += 1;
                } else {
                    self.perf.view_ms.push(ms(viewed - started));
                    self.perf.scene_ms.push(ms(built - viewed));
                    self.perf.encode_ms.push(ms(presented.encode));
                    self.perf.full_ms.push(cost);
                    self.perf.full_frames += 1;
                }
                self.perf.acquire_ms.push(ms(presented.acquire));
                self.perf.present_ms.push(ms(presented.present));
                self.perf.frame_ms.push(cost);
                self.perf.presented(now);
                if self.first_paint.is_none() {
                    trace("first present");
                }
                self.first_paint.get_or_insert_with(crate::perf::since_start);
                self.scene = Some(scene);
                // Moving loop dots change nothing UI Automation shows.
                self.access_due |= !loops_only;
            }
            Some(Err(Skip::Retry)) => {
                // Occluded or resized: try again (all layers), backing off while it lasts.
                self.perf.skipped += 1;
                self.retries += 1;
                self.redraw = true;
                let wait = if self.retries > 10 { 250 } else { 16 };
                self.retry_at = Some(now + Duration::from_millis(wait));
            }
            Some(Err(Skip::DeviceLost)) => {
                self.device_lost = true;
                self.redraw = true;
                wake();
            }
            None => {
                self.scene = None;
            }
        }
        if loops_only {
            // Nothing the core laid out changed: no subscriptions, sizes or IME position to send.
            return;
        }
        if !damage {
            // An open HTML report shows in the HTML view over its area (or the view hides).
            self.web.sync(hwnd_of(&self.window), self.view.html.as_ref(), self.grid.cell_w, self.grid.cell_h);
        }
        self.app.after_draw();
        self.app.raw_out.clear();
        self.tile_cache.retain(|id| self.app.views.contains_key(id));
        // The IME candidate window follows the focused terminal's cursor or text field.
        let field = self.view.cursor.filter(|_| ime.is_none()).map(|(x, y)| [x as f32 * self.grid.cell_w, y as f32 * self.grid.cell_h, self.grid.cell_w, self.grid.cell_h]);
        if let Some(r) = ime.or(field) {
            let rect = [r[0] as i32, r[1] as i32, r[2] as i32, r[3] as i32];
            if self.ime_rect != Some(rect) {
                self.window.set_ime_cursor_area(PhysicalPosition::new(rect[0], rect[1]), PhysicalSize::new(rect[2] as u32, rect[3] as u32));
                self.ime_rect = Some(rect);
            }
        }
    }

    /// After a DPI change: keeps the canvas's centre where it was.
    fn apply_recenter(&mut self) {
        let Some((center, until)) = self.recenter else { return };
        if Instant::now() >= until {
            self.recenter = None;
            return;
        }
        let now = self.app.canvas_center_f();
        if (now.0 - center.0).abs() > 1e-3 || (now.1 - center.1).abs() > 1e-3 {
            self.app.center_on_f(center.0, center.1);
            self.build_view();
        }
    }

    /// A new DPI scale (another monitor, or the `dpi` hook): cell metrics,
    /// the grid's size and the canvas centre.
    fn rescale(&mut self, scale: f32, advance: f32, gpu: Option<&Gpu>, options: &Options) {
        let center = self.app.canvas_center_f();
        self.grid = Grid::new(advance, scale);
        if let (true, Some((cols, rows))) = (options.test_hooks, options.size) {
            // A fixed grid: the window follows its new size.
            let size = PhysicalSize::new((cols as f32 * self.grid.cell_w) as u32, (rows as f32 * self.grid.cell_h) as u32);
            if let Some(now) = self.window.request_inner_size(size)
                && let (Some(gpu), Some(surface)) = (gpu, self.surface.as_mut())
            {
                surface.resize(gpu, now.width, now.height);
            }
        }
        let (width, height) = self.surface_size();
        self.fit(width, height, options);
        self.recenter = Some((center, Instant::now() + Duration::from_millis(800)));
        self.ime_rect = None;
        self.redraw = true;
        self.app.needs_draw = true;
    }

    /// Notes where the window is (saved a moment later).
    fn track_placement(&mut self) {
        if self.window.is_minimized() == Some(true) {
            return;
        }
        let maximized = self.window.is_maximized();
        if !maximized && let Ok(position) = self.window.outer_position() {
            let size = self.window.inner_size();
            self.placement.x = position.x;
            self.placement.y = position.y;
            self.placement.width = size.width;
            self.placement.height = size.height;
        }
        self.placement.maximized = maximized;
        self.placement.monitor = self.window.current_monitor().and_then(|m| m.name());
        self.placement_dirty = Some(Instant::now());
    }

    fn save_placement(&mut self) {
        self.placement_dirty = None;
        if self.placement.width == 0 || self.placement_path.as_os_str().is_empty() {
            return;
        }
        if crate::placement::load(&self.placement_path).as_ref() != Some(&self.placement)
            && let Err(error) = crate::placement::save(&self.placement_path, &self.placement)
        {
            eprintln!("copilot-canvas-gui: cannot save the window placement: {error:#}");
        }
    }

    /// The accessibility nodes of the current view.
    fn access_nodes(&self) -> (Vec<crate::access::Item>, u64) {
        crate::access::items(&self.app, &self.view, &self.chrome, &self.overlay, &self.grid, &self.window.title())
    }

    /// Sends the accessibility tree when a client listens and the UI changed.
    fn update_access(&mut self, force: bool) {
        if !self.access_active || !(self.access_due || force) || (!force && self.access_at.is_some_and(|t| t.elapsed() < ACCESS_EVERY)) {
            return;
        }
        let (items, focus) = self.access_nodes();
        if let Some(adapter) = self.access.as_mut() {
            adapter.update_if_active(|| crate::access::update(&items, focus));
        }
        self.access_items = items;
        self.access_due = false;
        self.access_at = Some(Instant::now());
    }

    /// Renders the core's view into the chrome and overlay buffers.
    fn build_view(&mut self) {
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
    }

    /// Every layer of a frame.
    fn build_scene(&mut self, fonts: &mut Fonts, options: &Options, width: u32, height: u32) -> (Scene, crate::tiles::CursorRect) {
        let mut scene = Scene::new(width, height, BG);
        let grid = self.grid;
        let mut p = Painter { scene: &mut scene, cache: &mut self.cache, fonts };
        if let Some(model) = &self.view.canvas {
            self.scene_clock = model.anim_clock();
            crate::canvas::paint(&mut p, &grid, model, self.scene_clock);
            self.loop_drawn = Instant::now();
            self.loop_interval = crate::canvas::loop_interval(&grid, model);
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
        // The text cursor of a field (terminals draw their own).
        if let Some((x, y)) = self.view.cursor.filter(|(x, y)| !inners.iter().any(|r| *x >= r.x && *x < r.right() && *y >= r.y && *y < r.bottom())) {
            let layer = if self.view.modal.is_some() { scene::OVERLAY } else { scene::CHROME_TOP };
            let thin = (2.0 * grid.scale).round().max(2.0);
            let full = [0.0, 0.0, width as f32, height as f32];
            p.over(layer, Quad::rect((x as f32 * grid.cell_w).round(), (y as f32 * grid.cell_h).round(), thin, grid.cell_h, theme::color(canvas_core::paint::TEXT, 1.0), full));
        }
        let ime = self.paint_dynamic(&mut scene, fonts, options, width, height);
        (scene, ime)
    }

    /// The layers a damage frame repaints: the terminal tiles and the HUD
    /// (IME composition, banner, paste preview, perf overlay).
    fn paint_dynamic(&mut self, scene: &mut Scene, fonts: &mut Fonts, options: &Options, width: u32, height: u32) -> crate::tiles::CursorRect {
        let grid = self.grid;
        let mut p = Painter { scene, cache: &mut self.cache, fonts };
        let input = TileInput {
            selection: self.app.term_select.as_ref(),
            links: &self.app.layout.links,
            hover: self.hover,
            ctrl: self.mods.control_key(),
            preedit: self.preedit.as_deref(),
        };
        let ime = crate::tiles::paint(&mut p, &grid, &self.view.tiles, &self.app.views, &mut self.tile_cache, &input);
        if let Some(text) = &self.view.banner {
            // Centred over the canvas (else the window).
            let center = self.view.canvas.as_ref().map(|c| (c.area.x as f32 + c.area.width as f32 / 2.0) * grid.cell_w).unwrap_or(width as f32 / 2.0);
            banner(&mut p, &grid, text, center, width);
        }
        if let Some(preview) = self.preview.as_mut() {
            // Top right of the focused terminal (else of the window), clear of its input line.
            let focused = self.view.tiles.iter().find(|t| t.focused).map(|t| t.inner);
            let right = focused.map(|r| r.right() as f32 * grid.cell_w).unwrap_or(width as f32) - grid.cell_w;
            let top = focused.map(|r| r.y as f32 * grid.cell_h).unwrap_or(grid.cell_h * 2.0) + grid.cell_h * 0.5;
            paste_preview(&mut p, &grid, preview, right, top, width, height);
        }
        if options.perf {
            let lines = self.perf.lines(self.app.conn.remote);
            hud(&mut p, &grid, &lines, width, self.rows);
        }
        ime
    }

    /// Renders the current frame offscreen to a PNG.
    fn png(&mut self, gpu: &mut Gpu, fonts: &mut Fonts, options: &Options, path: &std::path::Path) -> anyhow::Result<(u32, u32)> {
        if self.app.needs_draw || self.cols == 0 || self.redraw || !self.damaged.is_empty() {
            self.frame(gpu, fonts, options);
        }
        let (width, height) = self.grid_size();
        let (scene, _) = self.build_scene(fonts, options, width, height);
        let rgba = gpu.capture(&scene, &self.cache, fonts)?;
        crate::gpu::write_png(path, width, height, &rgba)?;
        Ok((width, height))
    }

    /// The loop arcs in view at `clock_ms` (test hooks): their box, lap,
    /// how much shows, and the lit dot in px and cells.
    fn loops_json(&self, clock_ms: u64) -> Value {
        let Some(model) = &self.view.canvas else { return json!([]) };
        json!(crate::canvas::loop_info(&self.grid, model, clock_ms))
    }

    /// When the loop layer is due again: loop dots in view move (or an arc
    /// grows in or fades out), every `loop_interval` and never faster
    /// than the display; `None` while nothing moves or the window is
    /// minimized.
    fn loops_due(&self) -> Option<Instant> {
        let model = self.view.canvas.as_ref()?;
        let size = self.window.inner_size();
        if self.surface.is_none() || size.width == 0 || size.height == 0 || !model.loops_busy(model.anim_clock()) {
            return None;
        }
        Some((self.loop_drawn + self.loop_interval).max(self.last_draw + self.frame_interval))
    }

    /// Builds the view for the hooks when something changed since the last frame.
    fn fresh_view(&mut self, gpu: Option<&mut Gpu>, fonts: Option<&mut Fonts>, options: &Options) {
        if !(self.app.needs_draw || self.redraw) {
            return;
        }
        match (gpu, fonts) {
            (Some(gpu), Some(fonts)) => self.frame(gpu, fonts, options),
            _ => self.build_view(),
        }
    }

    fn perf_json(&mut self, command: &Value) -> Value {
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

    /// One test-hook command for this window (`None`: unknown).
    fn hook(&mut self, command: &Value, options: &Options, gpu: Option<&mut Gpu>, fonts: Option<&mut Fonts>) -> Option<Value> {
        let name = command["cmd"].as_str().unwrap_or_default();
        let mods = parse_mods(&command["mods"]);
        let point = (command["x"].as_f64().unwrap_or(0.0) as f32, command["y"].as_f64().unwrap_or(0.0) as f32);
        Some(match name {
            "key" => match hook_key(command, mods) {
                Some(key) => {
                    self.key_input(key);
                    json!({ "ok": true })
                }
                None => json!({ "ok": false, "error": "unknown key" }),
            },
            "keys" => {
                // A batch, as one read of a console's input.
                let mut batch = Vec::new();
                for spec in command["keys"].as_array().into_iter().flatten() {
                    let Some(key) = hook_key(spec, parse_mods(&spec["mods"])) else { return Some(json!({ "ok": false, "error": format!("unknown key {spec}") })) };
                    if is_paste_key(&key) || is_alt_v(&key) || is_new_window_key(&key) {
                        if !batch.is_empty() {
                            self.raw_input(std::mem::take(&mut batch));
                        }
                        self.key_input(key);
                        continue;
                    }
                    batch.push(InputEvent::Key(key));
                }
                if !batch.is_empty() {
                    self.raw_input(batch);
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
                        self.raw_input(vec![InputEvent::Text(text.to_owned())]);
                    }
                    _ => {}
                }
                json!({ "ok": true })
            }
            "text" => {
                self.raw_input(vec![InputEvent::Text(command["text"].as_str().unwrap_or_default().to_owned())]);
                json!({ "ok": true })
            }
            "paste" => {
                self.raw_input(vec![InputEvent::Paste(command["text"].as_str().unwrap_or_default().to_owned())]);
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
                self.raw_input(vec![InputEvent::Mouse(canvas_core::input::MouseEvent::at(kind, point.0, point.1, mods))]);
                json!({ "ok": true })
            }
            "wheel" => {
                let kind = match command["dir"].as_str().unwrap_or("down") {
                    "up" => MouseEventKind::ScrollUp,
                    "left" => MouseEventKind::ScrollLeft,
                    "right" => MouseEventKind::ScrollRight,
                    _ => MouseEventKind::ScrollDown,
                };
                self.raw_input(vec![InputEvent::Mouse(canvas_core::input::MouseEvent::at(kind, point.0, point.1, mods))]);
                json!({ "ok": true })
            }
            "drop" => {
                // Files dropped at a cell, as from Explorer.
                let paths: Vec<String> = command["paths"].as_array().into_iter().flatten().filter_map(|p| p.as_str().map(str::to_owned)).collect();
                self.app.drop_files(paths, point);
                self.input_waiting = true;
                json!({ "ok": true })
            }
            "screen" => {
                self.fresh_view(gpu, fonts, options);
                json!({ "ok": true, "cols": self.cols, "rows": self.rows, "lines": crate::hooks::screen(&self.app, &self.view, &self.chrome, &self.overlay) })
            }
            "tree" => {
                self.fresh_view(gpu, fonts, options);
                let mut tree = crate::hooks::tree(&self.app, &self.view, &self.chrome);
                tree["window"] = json!(self.index);
                tree["paste_preview"] = json!(self.preview.as_ref().map(|p| json!({ "width": p.size.0, "height": p.size.1, "rect": p.item.rect })));
                if !tree["canvas"].is_null() {
                    tree["canvas"]["loop_arcs"] = self.loops_json(self.view.canvas.as_ref().map(|m| m.anim_clock()).unwrap_or_default());
                    tree["canvas"]["loop_interval_ms"] = json!(self.loop_interval.as_millis() as u64);
                }
                // The UI Automation tree, from the same builder AccessKit gets.
                let (items, focus) = self.access_nodes();
                tree["access"] = crate::access::json(&items, focus);
                tree["html"] = json!(self.view.html.as_ref().map(|h| json!({ "id": h.id, "rect": [h.rect.x, h.rect.y, h.rect.width, h.rect.height] })));
                json!({ "ok": true, "tree": tree })
            }
            "access" => {
                self.fresh_view(gpu, fonts, options);
                let (items, focus) = self.access_nodes();
                json!({ "ok": true, "active": self.access_active, "tree": crate::access::json(&items, focus) })
            }
            "place" => {
                // Moves, sizes or maximizes the window as the user would.
                if let Some(maximized) = command["maximized"].as_bool() {
                    self.window.set_maximized(maximized);
                }
                if let (Some(x), Some(y)) = (command["x"].as_i64(), command["y"].as_i64()) {
                    self.window.set_outer_position(PhysicalPosition::new(x as i32, y as i32));
                }
                if let (Some(w), Some(h)) = (command["width"].as_u64(), command["height"].as_u64()) {
                    let _ = self.window.request_inner_size(PhysicalSize::new(w as u32, h as u32));
                }
                json!({ "ok": true })
            }
            "panic_thread" => {
                // A crash on a helper thread (the crash-log check).
                let _ = std::thread::Builder::new().name("test-panic".into()).spawn(|| panic!("test hook asked for a crash on a helper thread"));
                json!({ "ok": true })
            }
            "clipboard" => {
                // The test hooks' clipboard (used when Windows' clipboard is unavailable).
                let Some(clip) = &self.platform.test_clipboard else { return Some(json!({ "ok": false, "error": "no test clipboard" })) };
                let mut clip = clip.lock().unwrap_or_else(|p| p.into_inner());
                if command["clear"].as_bool() == Some(true) {
                    *clip = Default::default();
                }
                if let Some(text) = command["text"].as_str() {
                    clip.system_unavailable = canvas_core::clipboard::set_text(text).is_err();
                    clip.text = Some(text.to_owned());
                }
                if let Some(image) = command.get("image") {
                    let (w, h) = (image["width"].as_u64().unwrap_or(1) as u32, image["height"].as_u64().unwrap_or(1) as u32);
                    let c: Vec<u8> = image["color"].as_array().map(|a| a.iter().filter_map(|v| v.as_u64().map(|v| v as u8)).collect()).unwrap_or_default();
                    let pixel = [c.first().copied().unwrap_or(0), c.get(1).copied().unwrap_or(0), c.get(2).copied().unwrap_or(0), 255];
                    clip.image = Some((w, h, pixel.repeat((w * h) as usize)));
                }
                json!({ "ok": true, "text": clip.text, "image": clip.image.is_some(), "system_unavailable": clip.system_unavailable })
            }
            "platform" => {
                let toasts = self.platform.toasts.lock().map(|t| t.clone()).unwrap_or_default();
                let clipboard = self.platform.test_clipboard.as_ref().map(|c| {
                    let c = c.lock().unwrap_or_else(|p| p.into_inner());
                    json!({ "text": c.text, "image": c.image.is_some(), "system_unavailable": c.system_unavailable })
                });
                json!({
                    "ok": true,
                    "title": self.window.title(),
                    "bells": self.platform.bells.load(Ordering::Relaxed),
                    "flashes": self.platform.flashes.load(Ordering::Relaxed),
                    "progress": self.platform.progress_state(),
                    "taskbar": self.taskbar.shown,
                    "toasts": toasts,
                    "clipboard": clipboard,
                    "toast_history": if command["history"].as_bool() == Some(true) { json!(crate::platform::toast_history()) } else { Value::Null },
                })
            }
            "size" => json!({
                "ok": true, "cols": self.cols, "rows": self.rows, "cell_w": self.grid.cell_w, "cell_h": self.grid.cell_h, "scale": self.grid.scale,
                "adapter": gpu.as_ref().map(|g| g.info.name.clone()), "adapter_details": gpu.as_ref().map(|g| g.describe()), "software": gpu.as_ref().map(|g| g.software),
                "present_mode": self.surface.as_ref().map(|s| format!("{:?}", s.present_mode())),
            }),
            "redraw" => {
                // Forces a full frame (measurements).
                self.app.needs_draw = true;
                if let (Some(gpu), Some(fonts)) = (gpu, fonts) {
                    self.frame(gpu, fonts, options);
                }
                json!({ "ok": true })
            }
            "freeze" => {
                // Animations hold frame `tick` (golden screenshots); without it they run again.
                self.app.frozen_tick = command["tick"].as_u64().map(|t| t as usize);
                if let Some(tick) = self.app.frozen_tick {
                    self.app.tick = tick;
                }
                self.app.needs_draw = true;
                json!({ "ok": true, "tick": self.app.tick })
            }
            "motion" => {
                // Settings › Effects for this window: "on", "reduced" or "off" (not saved).
                let motion = match command["value"].as_str().unwrap_or_default() {
                    "on" => canvas_core::view::Motion::On,
                    "reduced" => canvas_core::view::Motion::Reduced,
                    "off" => canvas_core::view::Motion::Off,
                    other => return Some(json!({ "ok": false, "error": format!("unknown motion {other:?}") })),
                };
                self.app.visual.motion = Some(motion);
                self.app.needs_draw = true;
                json!({ "ok": true, "motion": motion.name() })
            }
            "post_keys" => {
                // Real window messages, through winit's keyboard handling.
                let hwnd = hwnd_of(&self.window);
                let text = command["text"].as_str().unwrap_or_default().to_owned();
                let every = Duration::from_millis(command["interval_ms"].as_u64().unwrap_or(100));
                std::thread::spawn(move || {
                    for c in text.chars() {
                        crate::os::post_key(hwnd, c);
                        std::thread::sleep(every);
                    }
                });
                json!({ "ok": true })
            }
            "quit" => {
                self.app.quit = true;
                json!({ "ok": true })
            }
            "panic" => panic!("test hook asked for a crash"),
            _ => return None,
        })
    }

    /// A key as if typed: the window's own keys first, then the core.
    fn key_input(&mut self, key: KeyEvent) {
        let proxy = self.app_proxy();
        if !self.gui_key(&key, &proxy) {
            self.input(vec![InputEvent::Key(key)], &proxy);
        }
    }

    /// The HTML view's news reach the core: it is ready or failed (the
    /// viewer says so; a failure shows the report as formatted text), a
    /// clicked link (the canvas asks first), Esc (the report closes).
    fn apply_web(&mut self) {
        use canvas_core::reports::{ReportAction, WebState};
        for event in self.web.take_events() {
            match event {
                crate::os::webview::WebEvent::Ready => self.app.reports.web = WebState::Ready,
                crate::os::webview::WebEvent::Failed(why) => self.app.reports.web = WebState::Failed(why),
                crate::os::webview::WebEvent::Link(url) => self.app.report_action(ReportAction::Link(url)),
                crate::os::webview::WebEvent::Escape => self.app.modals.retain(|m| !matches!(m, canvas_core::dialogs::Modal::Report(_))),
            }
            self.app.needs_draw = true;
            self.input_waiting = true;
        }
    }

    fn raw_input(&mut self, events: Vec<InputEvent>) {
        let proxy = self.app_proxy();
        self.input(events, &proxy);
    }

    /// The proxy the window's threads use (kept by the forward threads; a
    /// fresh one is only needed when a connection is replaced).
    fn app_proxy(&self) -> EventLoopProxy<UserEvent> {
        PROXY.get().cloned().expect("event loop proxy")
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
            self.window.set_cursor(icon);
            self.cursor_icon = Some(icon);
        }
    }

    /// Saves before the window goes; the last edits reach the service (it
    /// saves them). Returns the save error, if any.
    fn finish(&mut self) -> Option<String> {
        self.save_placement();
        self.app.save_now();
        let deadline = Instant::now() + FLUSH_TIMEOUT;
        while self.app.replica.in_flight() > 0 && self.app.connected && Instant::now() < deadline {
            match self.svc_rx.recv_timeout(Duration::from_millis(50)) {
                Ok((generation, message)) if generation == self.svc_gen => self.app.on_service(message),
                Ok(_) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            }
        }
        self.app.save_error.clone()
    }

    fn mouse(&mut self, kind: MouseEventKind) {
        let event = crate::input::mouse(kind, self.pointer, self.mods);
        self.raw_input(vec![event]);
    }

    /// Files dropped since the last pass, at the pointer's cell.
    fn flush_dropped(&mut self) {
        if self.dropped.is_empty() {
            return;
        }
        let paths = std::mem::take(&mut self.dropped);
        let at = cursor_cell(&self.window, &self.grid).unwrap_or(self.pointer);
        self.app.drop_files(paths, at);
        self.input_waiting = true;
    }

    /// Housekeeping, then a frame when one is due; returns when to wake next.
    fn tick(&mut self, gpu: Option<&mut Gpu>, fonts: Option<&mut Fonts>, options: &Options) -> Instant {
        self.flush_dropped();
        let now = Instant::now();
        if now >= self.next_tick {
            let stats_at = self.app.stats_at;
            self.app.on_tick();
            if self.app.stats_at != stats_at {
                self.stats_sent = Some(self.app.stats_at);
            }
            if self.preview.as_ref().is_some_and(|p| now >= p.until) {
                self.preview = None;
                self.redraw = true;
            }
            let app = &self.app;
            // Effects alone tick on their own 80 ms clock (fewer wake-ups while they animate).
            let effects_only = app.visual.effects_live && !app.visual.animating();
            let busy = (app.needs_draw && !effects_only)
                || app.notice.is_some()
                || app.flash.is_some()
                || !app.pulses.is_empty()
                || app.dirty_at.is_some()
                || app.drag.is_some()
                || app.visual.animating()
                || app.recent_pending()
                || app.reconnecting
                || app.restarting.is_some()
                || self.preview.is_some();
            self.next_tick = now + if busy { TICK } else if effects_only { EFFECT_TICK } else { IDLE_TICK };
            let proxy = self.app_proxy();
            self.after_events(&proxy);
        }
        // Stack and hide transitions draw at the display's rate until they end.
        if self.app.visual.animating() {
            self.app.needs_draw = true;
        }
        self.platform.apply(&self.window, &mut self.taskbar);
        if self.placement_dirty.is_some_and(|t| t.elapsed() >= PLACEMENT_SAVE) {
            self.save_placement();
        }
        self.update_access(false);
        let due = |s: &Self| -> Option<Instant> {
            if !(s.app.needs_draw || s.redraw || !s.damaged.is_empty()) {
                // Moving loop dots: a frame of the loop layer alone.
                return s.loops_due();
            }
            if let Some(at) = s.retry_at {
                return Some(at);
            }
            Some(if s.input_waiting { Instant::now() } else { s.last_draw + s.frame_interval })
        };
        if due(self).is_some_and(|at| at <= Instant::now())
            && let (Some(gpu), Some(fonts)) = (gpu, fonts)
        {
            self.frame(gpu, fonts, options);
        }
        [Some(self.next_tick), due(self), self.placement_dirty.map(|t| t + PLACEMENT_SAVE), (self.access_active && self.access_due).then(|| self.access_at.map_or_else(Instant::now, |t| t + ACCESS_EVERY))]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(self.next_tick)
    }
}

/// The event loop's proxy, for window threads started after the first.
static PROXY: std::sync::OnceLock<EventLoopProxy<UserEvent>> = std::sync::OnceLock::new();

/// How long quitting waits for the last edits to reach the service.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(3);

/// Test-hook windows stay in the background (`CANVAS_GUI_TEST_VISIBLE=1`: a
/// normal window, e.g. to watch a test).
pub fn quiet_tests() -> bool {
    std::env::var_os("CANVAS_GUI_TEST_VISIBLE").is_none()
}

/// CANVAS_GUI_TRACE=1: start-up milestones on stderr (ms since process start).
fn trace(what: &str) {
    if std::env::var_os("CANVAS_GUI_TRACE").is_some() {
        eprintln!("{:>6} ms  {what}", crate::perf::since_start().as_millis());
    }
}

/// The window's native handle (HWND on Windows, NSView on macOS, the X11
/// window or Wayland surface on Linux) for the platform module.
fn hwnd_of(window: &Window) -> isize {
    crate::os::native_window(window)
}

/// The mouse pointer's cell in a window (files are dropped where it is).
fn cursor_cell(window: &Window, grid: &Grid) -> Option<(f32, f32)> {
    crate::os::cursor_position(window).map(|(x, y)| (x / grid.cell_w, y / grid.cell_h))
}


/// Ctrl+V or Shift+Insert.
fn is_paste_key(key: &KeyEvent) -> bool {
    let ctrl_v = key.modifiers.contains(Modifiers::CONTROL) && matches!(key.code, KeyCode::Char('v' | 'V'));
    ctrl_v || key.modifiers.contains(Modifiers::SHIFT) && key.code == KeyCode::Insert
}

/// Alt+V (Copilot's image paste key).
fn is_alt_v(key: &KeyEvent) -> bool {
    key.modifiers.contains(Modifiers::ALT) && !key.modifiers.contains(Modifiers::CONTROL) && matches!(key.code, KeyCode::Char('v' | 'V'))
}

fn is_new_window_key(key: &KeyEvent) -> bool {
    key.modifiers.contains(Modifiers::CONTROL | Modifiers::SHIFT) && matches!(key.code, KeyCode::Char('n' | 'N'))
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

/// The pasted image's thumbnail with its size, its top right corner at `right`, `top`.
fn paste_preview(p: &mut Painter, grid: &Grid, preview: &mut Preview, right: f32, top: f32, width: u32, height: u32) {
    let size = grid.font_px * 0.85;
    let caption = format!("Image attached · {} × {}", preview.size.0, preview.size.1);
    let (key, text_w) = p.shape(Spec::ui(&caption, size));
    let pad = grid.cell_w * 0.6;
    let (iw, ih) = (preview.item.width as f32, preview.item.height as f32);
    let w = iw.max(text_w) + 2.0 * pad;
    let h = ih + size * 1.4 + 3.0 * pad;
    let (x, y) = ((right - w).max(0.0), top.min(height as f32 - h).max(0.0));
    let full = [0.0, 0.0, width as f32, height as f32];
    p.under(scene::HUD, Quad::rounded(x, y, w, h, 6.0 * grid.scale, theme::rgba([12, 16, 24], 0.96), theme::color(canvas_core::paint::ACCENT, 1.0), grid.scale, full));
    let rect = [x + pad, y + pad, iw, ih];
    preview.item.rect = rect;
    preview.item.clip = full;
    p.image(scene::HUD, false, preview.item.clone());
    p.place(scene::HUD, key, x + pad, y + 2.0 * pad + ih, 1.0, theme::rgb(canvas_core::paint::TEXT, theme::FG), full);
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
        let _ = PROXY.set(self.proxy.clone());
        if !self.wins.is_empty() {
            return;
        }
        trace("resumed");
        for (opened, reply) in std::mem::take(&mut self.waiting) {
            match self.create_window(event_loop, opened) {
                Ok(index) => {
                    if let Some(reply) = reply {
                        let _ = reply.send(json!({ "ok": true, "window": index }));
                    }
                }
                Err(error) => {
                    let gpu = self.gpu.is_none();
                    if gpu {
                        // Neither a GPU nor WARP: the TUI needs neither.
                        eprintln!(
                            "copilot-canvas-gui: cannot draw: no usable GPU and no software renderer ({error:#}).\n\
                             Details are in the crash log. The terminal UI shows the same canvas and sessions without a GPU: run `copilot-canvas`."
                        );
                    } else {
                        eprintln!("copilot-canvas-gui: {error:#}");
                    }
                    self.exit_code = 1;
                    event_loop.exit();
                    return;
                }
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::GpuCheck => self.check_gpu(event_loop),
            UserEvent::Web => {
                for win in &mut self.wins {
                    win.apply_web();
                }
                self.after_input(event_loop);
            }
            UserEvent::Access(event) => {
                let Some(at) = self.by_window(event.window_id) else { return };
                let win = &mut self.wins[at];
                match event.window_event {
                    accesskit_winit::WindowEvent::InitialTreeRequested => {
                        win.access_active = true;
                        win.update_access(true);
                    }
                    accesskit_winit::WindowEvent::ActionRequested(request) => {
                        // Click, focus and the default action press the element.
                        let target = win.access_items.iter().find(|i| i.id == request.target_node.0).and_then(|i| i.click);
                        if let Some((x, y)) = target {
                            let mods = Modifiers::NONE;
                            let down = canvas_core::input::MouseEvent::at(MouseEventKind::Down(MouseButton::Left), x, y, mods);
                            let up = canvas_core::input::MouseEvent::at(MouseEventKind::Up(MouseButton::Left), x, y, mods);
                            win.raw_input(vec![InputEvent::Mouse(down), InputEvent::Mouse(up)]);
                        }
                        self.after_input(event_loop);
                    }
                    accesskit_winit::WindowEvent::AccessibilityDeactivated => win.access_active = false,
                }
            }
            UserEvent::Service(index) => {
                if let Some(at) = self.by_index(index) {
                    self.wins[at].drain_service(&self.proxy);
                }
            }
            UserEvent::ServiceClosed(index, generation) => {
                if let Some(at) = self.by_index(index) {
                    let win = &mut self.wins[at];
                    if generation == win.svc_gen && win.app.restarting.is_none() {
                        // Reconnects in the background; input is blocked meanwhile.
                        win.app.connection_lost();
                    }
                }
            }
            UserEvent::App(index, event) => {
                if let Some(at) = self.by_index(index) {
                    let win = &mut self.wins[at];
                    win.app.on_app_event(*event);
                    win.after_events(&self.proxy);
                }
                self.after_input(event_loop);
            }
            UserEvent::Hook(request) => {
                trace("hook");
                self.on_hook(event_loop, &request.command, request.reply);
            }
            UserEvent::Opened(result, reply) => {
                let answer = match *result {
                    Ok(opened) => match self.create_window(event_loop, opened) {
                        Ok(index) => json!({ "ok": true, "window": index }),
                        Err(error) => json!({ "ok": false, "error": format!("{error:#}") }),
                    },
                    Err(error) => {
                        if let Some(win) = self.wins.first_mut() {
                            win.app.notify(format!("Cannot open another window: {error}"), true);
                        }
                        json!({ "ok": false, "error": error })
                    }
                };
                if let Some(reply) = reply {
                    let _ = reply.send(answer);
                }
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(at) = self.by_window(id) else { return };
        let others = self.wins.len() > 1;
        let (options, proxy) = (&self.options, &self.proxy);
        let win = &mut self.wins[at];
        if let Some(adapter) = win.access.as_mut() {
            adapter.process_event(&win.window, &event);
        }
        match event {
            WindowEvent::CloseRequested => {
                if others {
                    // Another window stays open: this one just detaches.
                    win.app.quit = true;
                } else {
                    // As Ctrl+Q: asks to detach or stop the running sessions.
                    win.app.request_quit();
                }
                win.input_waiting = true;
            }
            WindowEvent::Resized(size) => {
                if let (Some(gpu), Some(surface)) = (&self.gpu, win.surface.as_mut()) {
                    surface.resize(gpu, size.width, size.height);
                }
                win.fit(size.width, size.height, options);
                win.redraw = true;
                win.retry_at = None;
                win.retries = 0;
                win.track_placement();
            }
            WindowEvent::Moved(_) => win.track_placement(),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // Another monitor's DPI: cell metrics and the grid follow; the
                // new window size arrives as `Resized`.
                let advance = self.fonts.as_ref().map(|f| f.advance).unwrap_or(0.6);
                win.rescale(scale_factor as f32, advance, self.gpu.as_ref(), options);
            }
            WindowEvent::RedrawRequested => win.redraw = true,
            WindowEvent::Focused(focused) => {
                win.focused = focused;
                win.input(vec![InputEvent::Focus(focused)], proxy);
            }
            WindowEvent::ModifiersChanged(m) => {
                let ctrl = win.mods.control_key() != m.state().control_key();
                win.mods = m.state();
                if ctrl && win.app.layout.links.iter().any(|l| l.terminal) {
                    win.redraw = true;
                }
            }
            WindowEvent::KeyboardInput { event, is_synthetic, .. } => {
                if is_synthetic || win.preedit.is_some() {
                    return;
                }
                if matches!(event.logical_key, Key::Named(NamedKey::Control | NamedKey::Shift | NamedKey::Alt | NamedKey::Super)) {
                    return;
                }
                let Some(input) = crate::input::key(&event.logical_key, event.state, win.mods) else { return };
                if let InputEvent::Key(key) = &input
                    && win.gui_key(key, proxy)
                {
                    self.after_input(event_loop);
                    return;
                }
                win.input(vec![input], proxy);
            }
            WindowEvent::Ime(ime) => match ime {
                Ime::Preedit(text, _) => {
                    win.preedit = (!text.is_empty()).then_some(text);
                    win.redraw = true;
                }
                Ime::Commit(text) => {
                    win.preedit = None;
                    win.input(vec![InputEvent::Text(text)], proxy);
                }
                Ime::Enabled | Ime::Disabled => {}
            },
            WindowEvent::CursorMoved { position, .. } => {
                let point = (position.x as f32 / win.grid.cell_w, position.y as f32 / win.grid.cell_h);
                let cell = (point.0.max(0.0) as u16, point.1.max(0.0) as u16);
                let moved_cell = (win.pointer.0 as u16, win.pointer.1 as u16) != cell;
                win.pointer = point;
                if let Some(&button) = win.held.last() {
                    win.mouse(MouseEventKind::Drag(button));
                } else if moved_cell {
                    // Hover: link underlines follow the pointer.
                    let over_link = |c: Option<(u16, u16)>| c.is_some_and(|(x, y)| win.app.layout.links.iter().any(|l| l.terminal && l.contains(x, y)));
                    if over_link(win.hover) || over_link(Some(cell)) {
                        win.redraw = true;
                    }
                    win.hover = Some(cell);
                    win.update_cursor(cell);
                }
            }
            WindowEvent::CursorLeft { .. } => win.hover = None,
            WindowEvent::MouseInput { state, button, .. } => {
                let Some(button) = crate::input::button(button) else { return };
                match state {
                    ElementState::Pressed => {
                        win.held.retain(|b| *b != button);
                        win.held.push(button);
                        win.mouse(MouseEventKind::Down(button));
                    }
                    ElementState::Released => {
                        win.held.retain(|b| *b != button);
                        win.mouse(MouseEventKind::Up(button));
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                for kind in win.wheel.notches(delta, win.grid.cell_h) {
                    win.mouse(kind);
                }
            }
            WindowEvent::DroppedFile(path) => win.dropped.push(path.display().to_string()),
            _ => {}
        }
        self.after_input(event_loop);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.wins.is_empty() {
            return;
        }
        self.check_gpu(event_loop);
        let mut wake: Option<Instant> = self.recover_at;
        for win in &mut self.wins {
            let next = win.tick(self.gpu.as_mut(), self.fonts.as_mut(), &self.options);
            wake = Some(wake.map_or(next, |w| w.min(next)));
        }
        if self.wins.iter().any(|w| w.device_lost) {
            self.check_gpu(event_loop);
        }
        if let Some(paint) = self.wins.first().and_then(|w| w.first_paint) {
            self.first_paint.get_or_insert(paint);
        }
        self.after_input(event_loop);
        if let Some(wake) = wake {
            event_loop.set_control_flow(ControlFlow::WaitUntil(wake));
        }
    }
}
