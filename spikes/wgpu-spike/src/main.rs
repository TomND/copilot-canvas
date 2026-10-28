//! Phase 3 spike: Copilot Canvas GPU client prototype on wgpu + winit + glyphon.
//!
//! Same scene, tiles, input and `--measure` protocol as the GPUI spike:
//! canvas (pan: drag or wheel; zoom: Ctrl+wheel around the cursor) next to four
//! live terminal tiles from a copilot-canvas service instance. F2 toggles the
//! tiles-only layout, F3 moves keyboard focus to the next tile. `--warp` (or
//! WGPU_FORCE_FALLBACK_ADAPTER=1) renders with the DX12 WARP software adapter.

#[path = "../../shared/keys.rs"]
mod keys;
#[path = "../../shared/measure.rs"]
mod measure;
mod quads;
#[path = "../../shared/scene.rs"]
mod scene;
#[path = "../../shared/service.rs"]
mod service;
#[path = "../../shared/term.rs"]
mod term;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use canvas_common::protocol::{Frame, Input};
use canvas_termview::TermView;
use glyphon::{
    Attrs, Buffer, Cache, Color, ColorMode, Family, FontSystem, Metrics, Resolution, Shaping, Style as FontStyle, SwashCache, TextArea, TextAtlas,
    TextBounds, TextRenderer, Viewport, Weight,
};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{Window, WindowId};

use measure::{Action, Driver};
use quads::{Quad, QuadPipeline, rgba};
use scene::{Camera, Rect, Scene, Visible};
use service::{Service, SvcEvent};
use term::{Layout, RowLayout, TileGeom};

struct Args {
    measure: bool,
    out: Option<String>,
    ime_demo: bool,
    altgr_demo: bool,
    warp: bool,
}

fn args() -> Args {
    let mut a = Args { measure: false, out: None, ime_demo: false, altgr_demo: false, warp: std::env::var_os("WGPU_FORCE_FALLBACK_ADAPTER").is_some_and(|v| v == "1") };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--measure" => a.measure = true,
            "--out" => a.out = it.next(),
            "--ime-demo" => a.ime_demo = true,
            "--altgr-demo" => a.altgr_demo = true,
            "--warp" => a.warp = true,
            other => eprintln!("unknown argument {other}"),
        }
    }
    a
}

enum UserEvent {
    Svc(SvcEvent),
}

#[derive(Default)]
struct TermState {
    view: TermView,
    rows: Vec<RowLayout>,
    frames: u64,
}

impl TermState {
    fn apply(&mut self, frame: Frame) {
        let full = frame.full || frame.rows != self.view.rows || frame.cols != self.view.cols;
        let changed: Vec<u16> = frame.changed.iter().map(|(y, _)| *y).collect();
        self.view.apply(frame);
        if full || self.rows.len() != self.view.lines.len() {
            self.rows = self.view.lines.iter().map(|runs| term::layout_row(runs)).collect();
        } else {
            for y in changed {
                if let Some(runs) = self.view.lines.get(y as usize) {
                    self.rows[y as usize] = term::layout_row(runs);
                }
            }
        }
        self.frames += 1;
    }
}

/// A shaped terminal text segment (cached by content + style + size).
struct SegBuf {
    buffer: Buffer,
    width: f32,
    used: u64,
}

struct Gpu {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    adapter: wgpu::AdapterInfo,
    quads: QuadPipeline,
    font_system: FontSystem,
    swash: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    canvas_text: TextRenderer,
    tile_text: TextRenderer,
    mono: String,
    advance: f32,
    labels: [Vec<Option<Buffer>>; 4],
    segs: HashMap<(u64, u32), SegBuf>,
    titles: HashMap<String, Buffer>,
    hud: Buffer,
    hud_text: String,
    preedit: Option<(String, Buffer)>,
    frame_no: u64,
}

impl Gpu {
    fn new(window: Arc<Window>, event_loop: &ActiveEventLoop, warp: bool, mut font_system: FontSystem, scene: &Scene) -> anyhow::Result<Self> {
        let mut desc = wgpu::InstanceDescriptor::new_with_display_handle(Box::new(event_loop.owned_display_handle()));
        desc.backends = wgpu::Backends::DX12;
        let instance = wgpu::Instance::new(desc);
        let surface = instance.create_surface(window.clone())?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::None,
            force_fallback_adapter: warp,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))?;
        // MemoryHints::MemoryUsage: small allocator blocks (the default,
        // Performance, reserves large blocks up front). SPIKE_WGPU_MEMORY=performance
        // restores the default for comparison.
        let memory_hints = match std::env::var("SPIKE_WGPU_MEMORY").as_deref() {
            Ok("performance") => wgpu::MemoryHints::Performance,
            _ => wgpu::MemoryHints::MemoryUsage,
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor { memory_hints, ..Default::default() }))?;
        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);
        let size = window.inner_size();
        // Default: Mailbox (present never blocks the UI thread) with our own
        // pacing to the display refresh, like GPUI's vsync thread + Present(0).
        // SPIKE_PRESENT=fifo uses FIFO, whose acquire blocks until vblank.
        let fifo = std::env::var("SPIKE_PRESENT").is_ok_and(|v| v == "fifo");
        let present_mode = if !fifo && caps.present_modes.contains(&wgpu::PresentMode::Mailbox) { wgpu::PresentMode::Mailbox } else { wgpu::PresentMode::AutoVsync };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: if fifo { 1 } else { 2 },
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &config);
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        // Colours are sRGB values written to a non-sRGB target (like GPUI and browsers).
        let mut atlas = TextAtlas::with_color_mode(&device, &queue, &cache, format, ColorMode::Web);
        let canvas_text = TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        let tile_text = TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        let quads = QuadPipeline::new(&device, format);
        let has = |name: &str| font_system.db().faces().any(|f| f.families.iter().any(|(n, _)| n == name));
        let mono = if has("Cascadia Mono") { "Cascadia Mono" } else { "Consolas" }.to_owned();
        let advance = {
            let mut b = Buffer::new(&mut font_system, Metrics::new(100.0, 120.0));
            b.set_text("MMMMMMMMMM", &Attrs::new().family(Family::Name(&mono)), Shaping::Advanced, None);
            b.shape_until_scroll(&mut font_system, false);
            b.layout_runs().next().map(|r| r.line_w / 1000.0).unwrap_or(0.6)
        };
        let hud = Buffer::new(&mut font_system, Metrics::new(12.0, 16.0));
        Ok(Gpu {
            window,
            surface,
            device,
            queue,
            config,
            adapter: adapter.get_info(),
            quads,
            font_system,
            swash: SwashCache::new(),
            viewport,
            atlas,
            canvas_text,
            tile_text,
            mono,
            advance,
            labels: [vec_none(scene.groups.len()), vec_none(scene.sessions.len()), vec_none(scene.sessions.len()), vec_none(scene.agents.len())],
            segs: HashMap::new(),
            titles: HashMap::new(),
            hud,
            hud_text: String::new(),
            preedit: None,
            frame_no: 0,
        })
    }

    fn resize(&mut self, size: PhysicalSize<u32>) {
        self.config.width = size.width.max(1);
        self.config.height = size.height.max(1);
        self.surface.configure(&self.device, &self.config);
    }
}

fn vec_none(n: usize) -> Vec<Option<Buffer>> {
    (0..n).map(|_| None).collect()
}

fn ui_attrs(bold: bool) -> Attrs<'static> {
    let a = Attrs::new().family(Family::Name("Segoe UI"));
    if bold { a.weight(Weight::BOLD) } else { a }
}

fn color(c: u32) -> Color {
    Color::rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

fn shape(fs: &mut FontSystem, text: &str, attrs: &Attrs, px: f32, line: f32) -> Buffer {
    let mut b = Buffer::new(fs, Metrics::new(px, line));
    b.set_size(None, None);
    b.set_text(text, attrs, Shaping::Advanced, None);
    b.shape_until_scroll(fs, false);
    b
}

fn line_width(b: &Buffer) -> f32 {
    b.layout_runs().map(|r| r.line_w).fold(0.0, f32::max)
}

struct App {
    args: Args,
    proxy: EventLoopProxy<UserEvent>,
    fonts: Option<std::thread::JoinHandle<FontSystem>>,
    gpu: Option<Gpu>,
    scene: Scene,
    camera: Camera,
    mode: Layout,
    animate_from: Option<Instant>,
    tiles: Vec<String>,
    terms: HashMap<String, TermState>,
    focused_tile: usize,
    preedit: Option<String>,
    mods: ModifiersState,
    cursor: (f32, f32),
    drag: Option<(f32, f32)>,
    svc: Option<Service>,
    driver: Option<Driver>,
    next_tick: Option<Instant>,
    retry_at: Option<Instant>,
    last_frame_at: Option<Instant>,
    frame_interval: Duration,
    demo_at: Option<(Instant, u8)>,
    status: String,
    visible: Visible,
    geoms: Option<(Rect, [TileGeom; 4])>,
    quads: Vec<Quad>,
    ime_area: Option<(i32, i32)>,
    hwnd: isize,
}

impl App {
    fn scale(&self) -> f32 {
        self.gpu.as_ref().map(|g| g.window.scale_factor() as f32).unwrap_or(1.0)
    }

    fn request_redraw(&self) {
        if let Some(gpu) = &self.gpu {
            gpu.window.request_redraw();
        }
    }

    fn connect(&mut self) {
        let proxy = self.proxy.clone();
        match Service::connect(move |ev| {
            let _ = proxy.send_event(UserEvent::Svc(ev));
        }) {
            Ok(svc) => {
                if let Err(e) = svc.launch_all() {
                    self.status = format!("launch failed: {e:#}");
                }
                self.tiles = vec![svc.ids.shell[0].clone(), svc.ids.noise[1].clone(), svc.ids.noise[2].clone(), svc.ids.noise[3].clone()];
                svc.subscribe(&self.tiles);
                self.status = format!("service {}", svc.build);
                self.svc = Some(svc);
            }
            Err(e) => self.status = format!("{e:#}"),
        }
    }

    fn send_input(&mut self, input: Input) {
        if let (Some(svc), Some(id)) = (&self.svc, self.tiles.get(self.focused_tile)) {
            if let Some(d) = &mut self.driver {
                d.forwarded();
            }
            svc.input(id, input);
        }
    }

    fn run_actions(&mut self, event_loop: &ActiveEventLoop) {
        let Some(driver) = &mut self.driver else { return };
        let actions = driver.take_actions();
        for action in actions {
            match action {
                Action::Tiles(ids) => {
                    self.tiles = ids;
                    if let Some(svc) = &self.svc {
                        svc.subscribe(&self.tiles);
                    }
                }
                Action::Layout(mode) => self.mode = mode,
                Action::Animate(on) => {
                    self.animate_from = on.then(Instant::now);
                    if !on {
                        self.camera = scene::initial_camera();
                    }
                }
                Action::Key(c) => {
                    self.focused_tile = 0;
                    keys::post_key(self.hwnd, c);
                }
                Action::Done(report) => {
                    println!("{report}");
                    if let Some(out) = &self.args.out {
                        let _ = std::fs::write(out, &report);
                    }
                    if let Some(svc) = &self.svc {
                        svc.stop_all();
                    }
                    std::thread::sleep(Duration::from_millis(300));
                    event_loop.exit();
                }
            }
            self.request_redraw();
        }
    }

    fn on_key(&mut self, event: &winit::event::KeyEvent) {
        if std::env::var_os("SPIKE_DEBUG_KEYS").is_some() {
            eprintln!("key {:?} text {:?} state {:?} mods {:?}", event.logical_key, event.text, event.state, self.mods);
        }
        if event.state != ElementState::Pressed {
            return;
        }
        match &event.logical_key {
            Key::Named(NamedKey::F2) => {
                self.mode = if self.mode == Layout::Normal { Layout::TilesOnly } else { Layout::Normal };
                self.request_redraw();
                return;
            }
            Key::Named(NamedKey::F3) => {
                self.focused_tile = (self.focused_tile + 1) % 4;
                self.request_redraw();
                return;
            }
            _ => {}
        }
        if self.preedit.is_some() {
            return;
        }
        let named = match &event.logical_key {
            Key::Named(n) => match n {
                NamedKey::Enter => Some(keys::Named::Enter),
                NamedKey::Tab => Some(keys::Named::Tab),
                NamedKey::Backspace => Some(keys::Named::Backspace),
                NamedKey::Escape => Some(keys::Named::Esc),
                NamedKey::ArrowUp => Some(keys::Named::Up),
                NamedKey::ArrowDown => Some(keys::Named::Down),
                NamedKey::ArrowLeft => Some(keys::Named::Left),
                NamedKey::ArrowRight => Some(keys::Named::Right),
                NamedKey::Home => Some(keys::Named::Home),
                NamedKey::End => Some(keys::Named::End),
                NamedKey::PageUp => Some(keys::Named::PageUp),
                NamedKey::PageDown => Some(keys::Named::PageDown),
                NamedKey::Insert => Some(keys::Named::Insert),
                NamedKey::Delete => Some(keys::Named::Delete),
                NamedKey::F1 => Some(keys::Named::F(1)),
                NamedKey::F4 => Some(keys::Named::F(4)),
                NamedKey::F5 => Some(keys::Named::F(5)),
                NamedKey::F6 => Some(keys::Named::F(6)),
                NamedKey::F7 => Some(keys::Named::F(7)),
                NamedKey::F8 => Some(keys::Named::F(8)),
                NamedKey::F9 => Some(keys::Named::F(9)),
                NamedKey::F10 => Some(keys::Named::F(10)),
                NamedKey::F11 => Some(keys::Named::F(11)),
                NamedKey::F12 => Some(keys::Named::F(12)),
                _ => None,
            },
            _ => None,
        };
        let base = match event.key_without_modifiers() {
            Key::Character(s) => s.chars().next(),
            Key::Named(NamedKey::Space) => Some(' '),
            _ => None,
        };
        let mods = keys::Mods { shift: self.mods.shift_key(), ctrl: self.mods.control_key(), alt: self.mods.alt_key() };
        if let Some(input) = keys::key_input(named, event.text.as_deref(), base, mods) {
            self.send_input(input);
        }
    }

    fn on_service(&mut self, ev: SvcEvent) {
        match ev {
            SvcEvent::Frame(frame) => {
                if let Some(d) = &mut self.driver {
                    d.service_frame(&frame);
                }
                let visible = self.tiles.contains(&frame.id);
                self.terms.entry(frame.id.clone()).or_default().apply(frame);
                if visible {
                    self.request_redraw();
                }
            }
            SvcEvent::Error(e) => {
                self.status = e;
                self.request_redraw();
            }
            SvcEvent::Closed => {
                self.status = "service connection closed".into();
                self.request_redraw();
            }
            SvcEvent::Terminals(_) => {}
        }
        if let (Some(d), Some(svc)) = (&mut self.driver, &self.svc)
            && !d.is_ready()
            && svc.ids.noise.iter().all(|id| self.terms.get(id).is_some_and(|t| t.frames > 0))
        {
            d.set_ready();
        }
    }

    fn render(&mut self) {
        let t_start = Instant::now();
        let Some(gpu) = self.gpu.as_mut() else { return };
        gpu.frame_no += 1;
        let frame_no = gpu.frame_no;
        let s = gpu.window.scale_factor() as f32;
        let (pw, ph) = (gpu.config.width, gpu.config.height);
        let win = Rect::new(0.0, 0.0, pw as f32 / s, ph as f32 / s);
        let (canvas_rect, geoms) = term::layout(win, self.mode, gpu.advance);
        self.geoms = Some((canvas_rect, geoms));
        if let Some(t0) = self.animate_from {
            self.camera = scene::animated_camera(t0.elapsed().as_secs_f64(), &self.scene.bounds, &canvas_rect);
        }
        let quads = &mut self.quads;
        quads.clear();
        let full_clip = [0.0, 0.0, pw as f32, ph as f32];
        let phys = |r: &Rect| [r.x * s, r.y * s, r.w * s, r.h * s];
        let clip_of = |r: &Rect| [r.x * s, r.y * s, (r.x + r.w) * s, (r.y + r.h) * s];
        let rect = |r: &Rect, fill: [f32; 4], radius: f32, bw: f32, border: [f32; 4], clip: [f32; 4]| Quad {
            rect: phys(r),
            color: fill,
            border,
            params: [radius * s, bw * s, 0.0, 0.0],
            clip,
        };
        quads.push(rect(&win, rgba(scene::BG, 1.0), 0.0, 0.0, [0.0; 4], full_clip));

        // ---- canvas -----------------------------------------------------
        let t_canvas = Instant::now();
        let cam = self.camera;
        let z = cam.zoom;
        let vp = canvas_rect;
        let cclip = clip_of(&vp);
        let scene = &self.scene;
        let visible = &mut self.visible;
        let mut canvas_areas_spec: Vec<(usize, usize, f32, f32, f32, u32)> = Vec::new();
        if vp.w > 0.0 {
            scene.visible(&cam.visible_world(&vp), visible);
            for &i in &visible.groups {
                let g = &scene.groups[i];
                let r = cam.rect_to_screen(&vp, &g.rect);
                quads.push(rect(&r, rgba(g.color, 0.04), 14.0 * z, 1.5, rgba(g.color, 0.8), cclip));
            }
            for &i in &visible.edges {
                let e = &scene.edges[i];
                let (ax, ay) = cam.to_screen(&vp, e.a);
                let (bx, by) = cam.to_screen(&vp, e.b);
                let w = (e.width * z).max(1.0) * s;
                let dash = if e.dashed { 6.0 * z.max(0.3) * s } else { 0.0 };
                quads.push(Quad { rect: [ax * s, ay * s, bx * s, by * s], color: rgba(e.color, 1.0), border: [0.0; 4], params: [w, 0.0, 1.0, dash], clip: cclip });
            }
            for &i in &visible.sessions {
                let ss = &scene.sessions[i];
                let r = cam.rect_to_screen(&vp, &ss.rect);
                quads.push(rect(&r, rgba(scene::SESSION_FILL, 1.0), 8.0 * z, (2.0 * z).max(1.0), rgba(ss.status.color(), 1.0), cclip));
            }
            for &i in &visible.agents {
                let a = &scene.agents[i];
                let r = cam.rect_to_screen(&vp, &a.rect);
                quads.push(rect(&r, rgba(scene::AGENT_FILL, 1.0), 5.0 * z, 1.0, rgba(scene::AGENT_BORDER, 1.0), cclip));
                if z > 0.25 {
                    let dot = Rect::new(r.x + 3.0 * z, r.y + r.h / 2.0 - 2.5 * z, 5.0 * z, 5.0 * z);
                    quads.push(rect(&dot, rgba(a.status.color(), 1.0), 2.5 * z, 0.0, [0.0; 4], cclip));
                }
            }
            // Labels: (kind, index, x, y, base px, colour); buffers shaped once at base size.
            for &i in &visible.groups {
                let g = &scene.groups[i];
                let (x, y) = cam.to_screen(&vp, (g.rect.x + 16.0, g.rect.y + 10.0));
                canvas_areas_spec.push((0, i, x, y, scene::GROUP_TITLE_PX, g.color));
            }
            for &i in &visible.sessions {
                let ss = &scene.sessions[i];
                let (x, y) = cam.to_screen(&vp, (ss.rect.x + 10.0, ss.rect.y + 8.0));
                canvas_areas_spec.push((1, i, x, y, scene::SESSION_TITLE_PX, scene::TEXT));
                let (x, y) = cam.to_screen(&vp, (ss.rect.x + 10.0, ss.rect.y + 32.0));
                canvas_areas_spec.push((2, i, x, y, scene::SESSION_STATUS_PX, ss.status.color()));
            }
            for &i in &visible.agents {
                let a = &scene.agents[i];
                let (x, y) = cam.to_screen(&vp, (a.rect.x + 12.0, a.rect.y + 4.0));
                canvas_areas_spec.push((3, i, x, y, scene::AGENT_PX, scene::TEXT_DIM));
            }
            canvas_areas_spec.retain(|spec| scene::text_px(spec.4, z) >= scene::MIN_TEXT_PX);
            for &(kind, i, ..) in &canvas_areas_spec {
                let slot = &mut gpu.labels[kind][i];
                if slot.is_none() {
                    let (text, px, bold) = match kind {
                        0 => (&scene.groups[i].title, scene::GROUP_TITLE_PX, true),
                        1 => (&scene.sessions[i].title, scene::SESSION_TITLE_PX, true),
                        2 => (&scene.sessions[i].status_line, scene::SESSION_STATUS_PX, false),
                        _ => (&scene.agents[i].label, scene::AGENT_PX, false),
                    };
                    *slot = Some(shape(&mut gpu.font_system, text, &ui_attrs(bold), px, px * 1.25));
                }
            }
            let hud = format!("wgpu · zoom {:.2} · {} boxes · {} connectors drawn · {} · {}", z, visible.boxes(), visible.edges.len(), gpu.adapter.name, self.status);
            if hud != gpu.hud_text {
                gpu.hud.set_text(&hud, &ui_attrs(false), Shaping::Advanced, None);
                gpu.hud.shape_until_scroll(&mut gpu.font_system, false);
                gpu.hud_text = hud;
            }
            let hud_w = line_width(&gpu.hud);
            quads.push(rect(&Rect::new(vp.x, vp.y, hud_w + 16.0, 22.0), rgba(0x000000, 0.55), 0.0, 0.0, [0.0; 4], cclip));
        }
        let canvas_end = quads.len() as u32;
        let canvas_ms = t_canvas.elapsed().as_secs_f64() * 1000.0;

        // ---- tiles ------------------------------------------------------
        let t_tiles = Instant::now();
        let mut cells = 0usize;
        // (buffer key, left, top, clip, colour) for every visible text segment.
        let mut seg_areas: Vec<((u64, u32), f32, f32, [f32; 4], u32)> = Vec::new();
        let mut overlays: Vec<Quad> = Vec::new();
        let mut title_specs: Vec<(String, f32, f32, [f32; 4])> = Vec::new();
        let mut preedit_spec: Option<(f32, f32, [f32; 4])> = None;
        let mut ime_area = None;
        for (index, geom) in geoms.iter().enumerate() {
            let focused = index == self.focused_tile;
            let id = self.tiles.get(index).cloned().unwrap_or_default();
            let border = if focused { 0x4fa3ff } else { 0x3a3d48 };
            quads.push(rect(&geom.outer, rgba(term::DEFAULT_BG, 1.0), 6.0, if focused { 2.0 } else { 1.0 }, rgba(border, 1.0), full_clip));
            title_specs.push((
                format!("{} · {}×{}{}", id, term::COLS, term::ROWS, if focused { " · keyboard focus" } else { "" }),
                geom.outer.x + 8.0,
                geom.outer.y + 2.0,
                clip_of(&geom.outer),
            ));
            let Some(state) = self.terms.get(&id) else { continue };
            let (cw, ch) = (geom.cell_w, geom.cell_h);
            let (vis_cols, vis_rows) = (geom.visible_cols(), geom.visible_rows());
            let g = geom.grid;
            let gclip = clip_of(&g);
            let first = term::first_row(state.rows.len(), vis_rows as usize, state.view.cursor);
            let size_key = (geom.font_px * s * 64.0).round() as u32;
            for (y, row) in state.rows.iter().skip(first).enumerate().take(vis_rows as usize) {
                let top = g.y + y as f32 * ch;
                for span in &row.bgs {
                    if span.col < vis_cols {
                        quads.push(rect(&Rect::new(g.x + span.col as f32 * cw, top, span.cells as f32 * cw, ch), rgba(span.color, 1.0), 0.0, 0.0, [0.0; 4], gclip));
                    }
                }
                for seg in &row.segs {
                    if seg.col >= vis_cols {
                        continue;
                    }
                    cells += seg.cells as usize;
                    let key = (seg.hash, size_key);
                    let entry = gpu.segs.entry(key).or_insert_with(|| {
                        let mut attrs = Attrs::new().family(Family::Name(&gpu.mono));
                        if seg.style.bold {
                            attrs = attrs.weight(Weight::BOLD);
                        }
                        if seg.style.italic {
                            attrs = attrs.style(FontStyle::Italic);
                        }
                        let buffer = shape(&mut gpu.font_system, &seg.text, &attrs, geom.font_px * s, ch * s);
                        let width = line_width(&buffer) / s;
                        SegBuf { buffer, width, used: 0 }
                    });
                    entry.used = frame_no;
                    let mut x = g.x + seg.col as f32 * cw;
                    if !seg.text.is_ascii() {
                        x += ((seg.cells as f32 * cw - entry.width) / 2.0).max(0.0);
                    }
                    seg_areas.push((key, x, top, gclip, seg.style.fg));
                }
                for (span, yy) in row.underlines.iter().map(|sp| (sp, top + ch - 2.0)).chain(row.strikes.iter().map(|sp| (sp, top + ch / 2.0))) {
                    overlays.push(rect(&Rect::new(g.x + span.col as f32 * cw, yy, span.cells as f32 * cw, 1.0), rgba(span.color, 1.0), 0.0, 0.0, [0.0; 4], gclip));
                }
            }
            if let Some((cx, cy)) = state.view.cursor
                && cx < vis_cols
                && (cy as usize) >= first
                && (cy as usize) < first + vis_rows as usize
            {
                let r = Rect::new(g.x + cx as f32 * cw, g.y + (cy as usize - first) as f32 * ch, cw, ch);
                if focused {
                    ime_area = Some((r, s));
                }
                match (&self.preedit, focused) {
                    (Some(text), true) => {
                        let width = text.chars().map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)).sum::<usize>() as f32 * cw;
                        let pr = Rect::new(r.x, r.y, width.max(cw), ch);
                        quads.push(rect(&pr, rgba(0x2d3350, 1.0), 0.0, 0.0, [0.0; 4], gclip));
                        overlays.push(rect(&Rect::new(pr.x, pr.y + ch - 2.0, pr.w, 1.5), rgba(0xffb02e, 1.0), 0.0, 0.0, [0.0; 4], gclip));
                        let stale = gpu.preedit.as_ref().is_none_or(|(t, _)| t != text);
                        if stale {
                            let attrs = Attrs::new().family(Family::Name(&gpu.mono));
                            gpu.preedit = Some((text.clone(), shape(&mut gpu.font_system, text, &attrs, geom.font_px * s, ch * s)));
                        }
                        preedit_spec = Some((r.x, r.y, gclip));
                    }
                    (_, true) => overlays.push(rect(&r, rgba(0x4fa3ff, 0.65), 0.0, 0.0, [0.0; 4], gclip)),
                    _ => overlays.push(rect(&r, [0.0; 4], 0.0, 1.0, rgba(0x8b949e, 1.0), gclip)),
                }
            }
        }
        for (title, ..) in &title_specs {
            if !gpu.titles.contains_key(title) {
                let b = shape(&mut gpu.font_system, title, &ui_attrs(false), 12.0 * s, 16.0 * s);
                gpu.titles.insert(title.clone(), b);
            }
        }
        let tiles_end = quads.len() as u32;
        quads.extend(overlays);
        let tiles_ms = t_tiles.elapsed().as_secs_f64() * 1000.0;

        // ---- text areas ---------------------------------------------------
        let bounds = |c: [f32; 4]| TextBounds { left: c[0] as i32, top: c[1] as i32, right: c[2].ceil() as i32, bottom: c[3].ceil() as i32 };
        let mut canvas_areas: Vec<TextArea> = Vec::with_capacity(canvas_areas_spec.len() + 1);
        for &(kind, i, x, y, base, col) in &canvas_areas_spec {
            if let Some(buffer) = &gpu.labels[kind][i] {
                canvas_areas.push(TextArea {
                    buffer,
                    left: x * s,
                    top: y * s,
                    scale: scene::text_px(base, z) / base * s,
                    bounds: bounds(cclip),
                    default_color: color(col),
                    custom_glyphs: &[],
                });
            }
        }
        if vp.w > 0.0 {
            canvas_areas.push(TextArea {
                buffer: &gpu.hud,
                left: (vp.x + 8.0) * s,
                top: (vp.y + 3.0) * s,
                scale: s,
                bounds: bounds(cclip),
                default_color: color(scene::TEXT),
                custom_glyphs: &[],
            });
        }
        let mut tile_areas: Vec<TextArea> = Vec::with_capacity(seg_areas.len() + 8);
        for (key, x, y, clip, col) in &seg_areas {
            tile_areas.push(TextArea { buffer: &gpu.segs[key].buffer, left: x * s, top: y * s, scale: 1.0, bounds: bounds(*clip), default_color: color(*col), custom_glyphs: &[] });
        }
        for (title, x, y, clip) in &title_specs {
            tile_areas.push(TextArea { buffer: &gpu.titles[title], left: x * s, top: y * s, scale: 1.0, bounds: bounds(*clip), default_color: color(scene::TEXT_DIM), custom_glyphs: &[] });
        }
        if let (Some((x, y, clip)), Some((_, buffer))) = (preedit_spec, &gpu.preedit) {
            tile_areas.push(TextArea { buffer, left: x * s, top: y * s, scale: 1.0, bounds: bounds(clip), default_color: color(0xffffff), custom_glyphs: &[] });
        }
        gpu.viewport.update(&gpu.queue, Resolution { width: pw, height: ph });
        let t_prep = Instant::now();
        let r1 = gpu.canvas_text.prepare(&gpu.device, &gpu.queue, &mut gpu.font_system, &mut gpu.atlas, &gpu.viewport, canvas_areas, &mut gpu.swash);
        let canvas_prepare_ms = t_prep.elapsed().as_secs_f64() * 1000.0;
        let r2 = gpu.tile_text.prepare(&gpu.device, &gpu.queue, &mut gpu.font_system, &mut gpu.atlas, &gpu.viewport, tile_areas, &mut gpu.swash);
        if let Err(e) = r1.and(r2) {
            self.status = format!("text prepare: {e:?}");
        }
        let tile_prepare_ms = t_prep.elapsed().as_secs_f64() * 1000.0 - canvas_prepare_ms;
        gpu.quads.upload(&gpu.device, &gpu.queue, quads, pw, ph);

        // ---- acquire, encode, present --------------------------------------
        let t_acquire = Instant::now();
        let frame = match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => frame,
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                self.retry_at = Some(Instant::now() + Duration::from_millis(16));
                return;
            }
            _ => {
                // Occluded or timed out: try again shortly instead of spinning.
                self.retry_at = Some(Instant::now() + Duration::from_millis(16));
                return;
            }
        };
        let acquire_ms = t_acquire.elapsed().as_secs_f64() * 1000.0;
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("frame"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            // SPIKE_SKIP=text|quads drops one kind of draw (diagnostics only).
            static SKIP: std::sync::OnceLock<String> = std::sync::OnceLock::new();
            let skip = SKIP.get_or_init(|| std::env::var("SPIKE_SKIP").unwrap_or_default());
            let (quads_on, text_on) = (skip != "quads", skip != "text");
            if quads_on {
                gpu.quads.draw(&mut pass, 0..canvas_end);
            }
            if text_on {
                let _ = gpu.canvas_text.render(&gpu.atlas, &gpu.viewport, &mut pass);
            }
            if quads_on {
                gpu.quads.draw(&mut pass, canvas_end..tiles_end);
            }
            if text_on {
                let _ = gpu.tile_text.render(&gpu.atlas, &gpu.viewport, &mut pass);
            }
            if quads_on {
                gpu.quads.draw(&mut pass, tiles_end..quads.len() as u32);
            }
        }
        gpu.queue.submit(Some(encoder.finish()));
        gpu.window.pre_present_notify();
        gpu.queue.present(frame);
        gpu.atlas.trim();
        // Shaped segments not used for a while are dropped.
        if gpu.segs.len() > 20_000 || frame_no % 120 == 0 {
            gpu.segs.retain(|_, seg| frame_no - seg.used < 120);
        }
        let now = Instant::now();
        let frame_ms = (now - t_start).as_secs_f64() * 1000.0 - acquire_ms;
        if let Some((r, s)) = ime_area {
            let pos = ((r.x * s) as i32, (r.y * s) as i32);
            if self.ime_area != Some(pos) {
                gpu.window.set_ime_cursor_area(PhysicalPosition::new(pos.0, pos.1), PhysicalSize::new((r.w * s) as u32, (r.h * s) as u32));
                self.ime_area = Some(pos);
            }
        }
        if let Some(d) = &mut self.driver {
            d.presented(frame_ms, acquire_ms, now);
            d.drawn(self.visible.boxes() * (vp.w > 0.0) as usize, cells, canvas_ms + canvas_prepare_ms, tiles_ms + tile_prepare_ms);
        }
        if self.animate_from.is_some() {
            gpu.window.request_redraw();
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gpu.is_some() {
            return;
        }
        let attrs = Window::default_attributes().with_title("Copilot Canvas — wgpu spike").with_inner_size(LogicalSize::new(1800.0, 1000.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        window.set_ime_allowed(true);
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if let Ok(handle) = window.window_handle()
            && let RawWindowHandle::Win32(h) = handle.as_raw()
        {
            self.hwnd = h.hwnd.get();
        }
        let fonts = self.fonts.take().map(|h| h.join().expect("font loading")).unwrap_or_else(FontSystem::new);
        if let Some(d) = &mut self.driver {
            d.memory_checkpoint("after_window_and_fonts");
        }
        match Gpu::new(window, event_loop, self.args.warp, fonts, &self.scene) {
            Ok(gpu) => {
                if let Some(mhz) = gpu.window.current_monitor().and_then(|m| m.refresh_rate_millihertz()).filter(|m| *m > 0) {
                    self.frame_interval = Duration::from_secs_f64(1000.0 / mhz as f64);
                }
                if let Some(d) = &mut self.driver {
                    d.memory_checkpoint("after_gpu_init");
                    d.info("refresh_hz", serde_json::json!(measure::round2(1.0 / self.frame_interval.as_secs_f64())));
                    d.info("adapter", serde_json::json!(gpu.adapter.name));
                    d.info("adapter_type", serde_json::json!(format!("{:?}", gpu.adapter.device_type)));
                    d.info("backend", serde_json::json!(format!("{:?}", gpu.adapter.backend)));
                    d.info("software_renderer", serde_json::json!(gpu.adapter.device_type == wgpu::DeviceType::Cpu));
                    d.info("scale_factor", serde_json::json!(gpu.window.scale_factor()));
                    d.info("window_logical", serde_json::json!([1800, 1000]));
                    d.info("mono_font", serde_json::json!(gpu.mono));
                    d.info("present_mode", serde_json::json!(format!("{:?}", gpu.config.present_mode)));
                }
                gpu.window.request_redraw();
                self.gpu = Some(gpu);
            }
            Err(e) => {
                eprintln!("GPU init failed: {e:#}");
                event_loop.exit();
                return;
            }
        }
        self.connect();
        if self.args.ime_demo || self.args.altgr_demo {
            self.demo_at = Some((Instant::now() + Duration::from_secs(4), 0));
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Svc(ev) => self.on_service(ev),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                if let Some(svc) = &self.svc {
                    svc.stop_all();
                    std::thread::sleep(Duration::from_millis(200));
                }
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(gpu) = &mut self.gpu {
                    gpu.resize(size);
                }
                self.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { .. } => self.request_redraw(),
            WindowEvent::RedrawRequested => {
                // Mailbox pacing: at most one frame per display refresh.
                let paced = self.gpu.as_ref().is_some_and(|g| g.config.present_mode == wgpu::PresentMode::Mailbox);
                let now = Instant::now();
                if paced && let Some(last) = self.last_frame_at {
                    let due = last + self.frame_interval;
                    if now < due {
                        self.retry_at = Some(self.retry_at.map_or(due, |r| r.min(due)));
                        return;
                    }
                }
                self.last_frame_at = Some(now);
                self.render();
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event, .. } => self.on_key(&event),
            WindowEvent::Ime(ime) => match ime {
                Ime::Preedit(text, _) => {
                    self.preedit = (!text.is_empty()).then_some(text);
                    self.request_redraw();
                }
                Ime::Commit(text) => {
                    self.preedit = None;
                    if let Some(input) = keys::text_input(&text) {
                        self.send_input(input);
                    }
                    self.request_redraw();
                }
                Ime::Enabled | Ime::Disabled => {}
            },
            WindowEvent::CursorMoved { position, .. } => {
                let s = self.scale();
                let p = (position.x as f32 / s, position.y as f32 / s);
                if let Some(last) = self.drag {
                    self.camera.pan(p.0 - last.0, p.1 - last.1);
                    self.drag = Some(p);
                    self.request_redraw();
                }
                self.cursor = p;
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                if state == ElementState::Released {
                    self.drag = None;
                } else if let Some((canvas, geoms)) = &self.geoms {
                    let (x, y) = self.cursor;
                    if let Some(i) = geoms.iter().position(|g| g.outer.contains(x, y)) {
                        self.focused_tile = i;
                        self.request_redraw();
                    } else if canvas.contains(x, y) {
                        self.drag = Some((x, y));
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let Some((canvas, _)) = self.geoms else { return };
                let (x, y) = self.cursor;
                if !canvas.contains(x, y) {
                    return;
                }
                let s = self.scale();
                let (dx, dy, lines) = match delta {
                    MouseScrollDelta::LineDelta(dx, dy) => (dx * 40.0, dy * 40.0, dy),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32 / s, p.y as f32 / s, p.y as f32 / 50.0),
                };
                if self.mods.control_key() {
                    self.camera.zoom_at(&canvas, (x, y), 1.15f32.powf(lines));
                } else {
                    self.camera.pan(dx, dy);
                }
                self.request_redraw();
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        if self.driver.is_some() && self.next_tick.is_none_or(|t| now >= t) {
            let next = self.driver.as_mut().map(|d| d.tick(now));
            self.next_tick = next;
            self.run_actions(event_loop);
        }
        if let Some(at) = self.retry_at
            && now >= at
        {
            self.retry_at = None;
            self.request_redraw();
        }
        if let Some((at, step)) = self.demo_at
            && now >= at
        {
            self.demo_step(step);
        }
        let mut wake = [self.next_tick, self.retry_at, self.demo_at.map(|d| d.0)].into_iter().flatten().min();
        if self.driver.is_some() {
            wake = wake.map(|w| w.max(now));
        }
        event_loop.set_control_flow(match wake {
            Some(at) => ControlFlow::WaitUntil(at),
            None => ControlFlow::Wait,
        });
    }
}

impl App {
    /// `--ime-demo` / `--altgr-demo`: synthesized input on the focused tile.
    fn demo_step(&mut self, step: u8) {
        self.demo_at = None;
        if self.args.ime_demo {
            // The events winit delivers for an IME composition, fed to the same handler.
            let event = if step == 0 { Ime::Preedit("にほんご".into(), Some((0, 12))) } else { Ime::Commit("echo 日本語 ✓ 🙂".into()) };
            match event {
                Ime::Preedit(text, _) => self.preedit = Some(text),
                Ime::Commit(text) => {
                    self.preedit = None;
                    if let Some(input) = keys::text_input(&text) {
                        self.send_input(input);
                    }
                }
                _ => {}
            }
            if step == 0 {
                self.demo_at = Some((Instant::now() + Duration::from_secs(3), 1));
            }
        } else {
            // AltGr: "echo " typed normally, then AltGr+Q/E/7/0 (@ € { }) as the
            // message sequence a layout with AltGr produces (Ctrl+Alt+RAlt held,
            // WM_KEYDOWN, WM_CHAR), then the modifiers released.
            match step {
                0 => {
                    for c in "echo ".chars() {
                        keys::post_key(self.hwnd, c);
                    }
                    self.demo_at = Some((Instant::now() + Duration::from_millis(400), 1));
                }
                1 => {
                    for (key, c) in [('q', '@'), ('e', '€'), ('7', '{'), ('0', '}')] {
                        keys::post_altgr(self.hwnd, key, c);
                    }
                    self.demo_at = Some((Instant::now() + Duration::from_millis(400), 2));
                }
                _ => keys::release_altgr(),
            }
        }
        self.request_redraw();
    }
}

fn main() -> anyhow::Result<()> {
    let args = args();
    // System fonts load on a thread while the window and the GPU device start.
    let fonts = std::thread::spawn(FontSystem::new);
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let driver = args.measure.then(|| {
        let ids = service::Ids::new();
        let mut d = Driver::new("wgpu", measure::Ids { noise: ids.noise.to_vec(), shell: ids.shell.to_vec(), echo: ids.echo });
        d.memory_checkpoint("at_start");
        d
    });
    let mut app = App {
        proxy: event_loop.create_proxy(),
        args,
        fonts: Some(fonts),
        gpu: None,
        scene: scene::build(),
        camera: scene::initial_camera(),
        mode: Layout::Normal,
        animate_from: None,
        tiles: Vec::new(),
        terms: HashMap::new(),
        focused_tile: 0,
        preedit: None,
        mods: ModifiersState::empty(),
        cursor: (0.0, 0.0),
        drag: None,
        svc: None,
        driver,
        next_tick: None,
        retry_at: None,
        last_frame_at: None,
        frame_interval: Duration::from_micros(16_667),
        demo_at: None,
        status: "connecting…".into(),
        visible: Visible::default(),
        geoms: None,
        quads: Vec::new(),
        ime_area: None,
        hwnd: 0,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}
