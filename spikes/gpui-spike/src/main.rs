//! Phase 3 spike: Copilot Canvas GPU client prototype on GPUI.
//!
//! A canvas of 30 groups, 300 sessions and 3,000 subagents (pan: drag or
//! wheel; zoom: Ctrl+wheel around the cursor) next to four live terminal tiles
//! streamed from a copilot-canvas service instance. F2 toggles the
//! tiles-only layout, F3 moves keyboard focus to the next tile.
//! `--measure` runs the automated measurement and prints one JSON object.
//!
//! Structure: the root view (`Spike`) owns tiles, input and the service
//! connection; the canvas is its own entity rendered as a *cached* view, so
//! frames caused by terminal output reuse the canvas's previous paint.

#[path = "../../shared/keys.rs"]
mod keys;
#[path = "../../shared/measure.rs"]
mod measure;
#[path = "../../shared/scene.rs"]
mod scene;
#[path = "../../shared/service.rs"]
mod service;
#[path = "../../shared/term.rs"]
mod term;

use std::collections::HashMap;
use std::io::BufRead;
use std::ops::Range;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use canvas_common::protocol::{Frame, Input};
use canvas_termview::TermView;
use futures::StreamExt;
use gpui::{
    App, BorderStyle, Bounds, ContentMask, Context, ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Font, FontStyle, FontWeight,
    Hsla, KeyDownEvent, LineLayout, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathBuilder, Pixels, Point, Render,
    ScrollDelta, ScrollEvent, StyleRefinement, TextAlign, TextRun, UTF16Selection, Window, WindowBounds, WindowOptions, canvas, div, fill,
    font, point, prelude::*, px, quad, size,
};

use measure::{Action, Driver};
use scene::{Camera, Rect, Scene, Visible};
use service::{Service, SvcEvent};
use term::{Layout, RowLayout, TileGeom};

type SharedDriver = Option<Arc<Mutex<Driver>>>;

struct Args {
    measure: bool,
    out: Option<String>,
    ime_demo: bool,
    altgr_demo: bool,
    warp: bool,
    uncached: bool,
}

fn args() -> Args {
    let mut a = Args { measure: false, out: None, ime_demo: false, altgr_demo: false, warp: false, uncached: false };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--measure" => a.measure = true,
            "--out" => a.out = it.next(),
            "--ime-demo" => a.ime_demo = true,
            "--altgr-demo" => a.altgr_demo = true,
            "--warp" => a.warp = true,
            // Repaint the canvas every frame (no GPUI view caching), for comparison.
            "--uncached" => a.uncached = true,
            other => eprintln!("unknown argument {other}"),
        }
    }
    a
}

fn hsla(c: u32) -> Hsla {
    gpui::rgb(c).into()
}

fn hsla_a(c: u32, alpha: f32) -> Hsla {
    let mut h: Hsla = gpui::rgb(c).into();
    h.a = alpha;
    h
}

fn bounds(r: &Rect) -> Bounds<Pixels> {
    Bounds::new(point(px(r.x), px(r.y)), size(px(r.w), px(r.h)))
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// A terminal as the client sees it: the L2 `TermView` plus the cell layout
/// of each row (recomputed only for rows a frame changed).
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

// ---- canvas view -------------------------------------------------------------

/// Canvas labels are shaped once at their base size and painted glyph by
/// glyph at the zoomed size (no reshaping while zooming).
struct CanvasView {
    scene: Rc<Scene>,
    labels: [Vec<Option<Arc<LineLayout>>>; 4],
    ui: (Font, Font),
    camera: Camera,
    animate_from: Option<Instant>,
    visible: Visible,
    status: String,
    /// Stats of the last paint (taken by the root each frame).
    boxes: usize,
    paint_ms: f64,
}

impl CanvasView {
    fn new() -> Self {
        let scene = Rc::new(scene::build());
        let labels = [vec![None; scene.groups.len()], vec![None; scene.sessions.len()], vec![None; scene.sessions.len()], vec![None; scene.agents.len()]];
        let ui = font("Segoe UI");
        CanvasView {
            scene,
            labels,
            ui: (ui.clone(), ui.bold()),
            camera: scene::initial_camera(),
            animate_from: None,
            visible: Visible::default(),
            status: String::new(),
            boxes: 0,
            paint_ms: 0.0,
        }
    }

    fn paint(&mut self, b: Bounds<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let t0 = Instant::now();
        let vp = Rect::new(b.origin.x.as_f32(), b.origin.y.as_f32(), b.size.width.as_f32(), b.size.height.as_f32());
        if let Some(t) = self.animate_from {
            self.camera = scene::animated_camera(t.elapsed().as_secs_f64(), &self.scene.bounds, &vp);
        }
        window.with_content_mask(Some(ContentMask { bounds: b }), |window| self.paint_inner(&vp, window, cx));
        self.paint_ms += ms(t0.elapsed());
    }

    fn paint_inner(&mut self, vp: &Rect, window: &mut Window, cx: &mut Context<Self>) {
        let cam = self.camera;
        let scene = self.scene.clone();
        let mut visible = std::mem::take(&mut self.visible);
        scene.visible(&cam.visible_world(vp), &mut visible);
        let z = cam.zoom;
        window.paint_quad(fill(bounds(vp), hsla(scene::BG)));
        // Groups: large rounded outlines.
        for &i in &visible.groups {
            let g = &scene.groups[i];
            let r = cam.rect_to_screen(vp, &g.rect);
            window.paint_quad(quad(bounds(&r), px(14.0 * z), hsla_a(g.color, 0.04), px(1.5), hsla_a(g.color, 0.8), BorderStyle::Solid));
        }
        // Connectors: elbows as thin quads (dashed borders), diagonals as stroked paths.
        for &i in &visible.edges {
            let e = &scene.edges[i];
            let (ax, ay) = cam.to_screen(vp, e.a);
            let (bx, by) = cam.to_screen(vp, e.b);
            let w = (e.width * z).max(1.0);
            if e.axis_aligned() {
                let r = if ay == by { Rect::new(ax.min(bx), ay - w / 2.0, (bx - ax).abs(), w) } else { Rect::new(ax - w / 2.0, ay.min(by), w, (by - ay).abs()) };
                if e.dashed {
                    // GPUI's dashed borders need a box; a thin connector is drawn as dash quads.
                    let horizontal = ay == by;
                    let len = if horizontal { r.w } else { r.h };
                    let (dash, gap) = (6.0 * z.max(0.3), 4.0 * z.max(0.3));
                    let mut t = 0.0;
                    while t < len {
                        let l = dash.min(len - t);
                        let d = if horizontal { Rect::new(r.x + t, r.y, l, r.h) } else { Rect::new(r.x, r.y + t, r.w, l) };
                        window.paint_quad(fill(bounds(&d), hsla(e.color)));
                        t += dash + gap;
                    }
                } else {
                    window.paint_quad(fill(bounds(&r), hsla(e.color)));
                }
            } else {
                let mut builder = PathBuilder::stroke(px(w));
                if e.dashed {
                    builder = builder.dash_array(&[px(6.0 * z.max(0.3)), px(4.0 * z.max(0.3))]);
                }
                builder.move_to(point(px(ax), px(ay)));
                builder.line_to(point(px(bx), px(by)));
                if let Ok(path) = builder.build() {
                    window.paint_path(path, hsla(e.color));
                }
            }
        }
        for &i in &visible.sessions {
            let s = &scene.sessions[i];
            let r = cam.rect_to_screen(vp, &s.rect);
            window.paint_quad(quad(bounds(&r), px(8.0 * z), hsla(scene::SESSION_FILL), px((2.0 * z).max(1.0)), hsla(s.status.color()), BorderStyle::Solid));
        }
        for &i in &visible.agents {
            let a = &scene.agents[i];
            let r = cam.rect_to_screen(vp, &a.rect);
            window.paint_quad(quad(bounds(&r), px(5.0 * z), hsla(scene::AGENT_FILL), px(1.0), hsla(scene::AGENT_BORDER), BorderStyle::Solid));
            if z > 0.25 {
                let dot = Rect::new(r.x + 3.0 * z, r.y + r.h / 2.0 - 2.5 * z, 5.0 * z, 5.0 * z);
                window.paint_quad(quad(bounds(&dot), px(2.5 * z), hsla(a.status.color()), px(0.0), gpui::transparent_black(), BorderStyle::Solid));
            }
        }
        let (ui, ui_bold) = self.ui.clone();
        for &i in &visible.groups {
            let g = &scene.groups[i];
            let (x, y) = cam.to_screen(vp, (g.rect.x + 16.0, g.rect.y + 10.0));
            let layout = label(&mut self.labels[0][i], &g.title, scene::GROUP_TITLE_PX, &ui_bold, window);
            paint_label(window, &layout, x, y, z, hsla(g.color));
        }
        for &i in &visible.sessions {
            let s = &scene.sessions[i];
            let (x, y) = cam.to_screen(vp, (s.rect.x + 10.0, s.rect.y + 8.0));
            let layout = label(&mut self.labels[1][i], &s.title, scene::SESSION_TITLE_PX, &ui_bold, window);
            paint_label(window, &layout, x, y, z, hsla(scene::TEXT));
            let (x, y) = cam.to_screen(vp, (s.rect.x + 10.0, s.rect.y + 32.0));
            let layout = label(&mut self.labels[2][i], &s.status_line, scene::SESSION_STATUS_PX, &ui, window);
            paint_label(window, &layout, x, y, z, hsla(s.status.color()));
        }
        if scene::text_px(scene::AGENT_PX, z) >= scene::MIN_TEXT_PX {
            for &i in &visible.agents {
                let a = &scene.agents[i];
                let (x, y) = cam.to_screen(vp, (a.rect.x + 12.0, a.rect.y + 4.0));
                let layout = label(&mut self.labels[3][i], &a.label, scene::AGENT_PX, &ui, window);
                paint_label(window, &layout, x, y, z, hsla(scene::TEXT_DIM));
            }
        }
        let hud = format!("GPUI · zoom {:.2} · {} boxes · {} connectors drawn · {}", z, visible.boxes(), visible.edges.len(), self.status);
        let run = TextRun { len: hud.len(), font: ui, color: hsla(scene::TEXT), background_color: None, underline: None, strikethrough: None };
        let line = window.text_system().shape_line(hud.into(), px(12.0), &[run], None);
        window.paint_quad(fill(Bounds::new(point(px(vp.x), px(vp.y)), size(line.width() + px(16.0), px(22.0))), hsla_a(0x000000, 0.55)));
        let _ = line.paint(point(px(vp.x + 8.0), px(vp.y + 3.0)), px(16.0), TextAlign::Left, None, window, cx);
        self.boxes = visible.boxes();
        self.visible = visible;
    }
}

impl Render for CanvasView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.animate_from.is_some() {
            window.request_animation_frame();
        }
        let entity = cx.entity();
        canvas(|_, _, _| {}, move |b, _, window, cx| entity.update(cx, |view, cx| view.paint(b, window, cx))).size_full()
    }
}

fn label(slot: &mut Option<Arc<LineLayout>>, text: &str, base: f32, f: &Font, window: &mut Window) -> Arc<LineLayout> {
    slot.get_or_insert_with(|| {
        let run = TextRun { len: text.len(), font: f.clone(), color: hsla(scene::TEXT), background_color: None, underline: None, strikethrough: None };
        window.text_system().layout_line(text, px(base), &[run], None)
    })
    .clone()
}

fn paint_label(window: &mut Window, layout: &LineLayout, x: f32, y: f32, zoom: f32, color: Hsla) {
    let base = layout.font_size.as_f32();
    let size = scene::text_px(base, zoom);
    if size < scene::MIN_TEXT_PX {
        return;
    }
    let scale = size / base;
    let ascent = layout.ascent.as_f32() * scale;
    for run in &layout.runs {
        for g in &run.glyphs {
            let origin = point(px(x + g.position.x.as_f32() * scale), px(y + ascent + g.position.y.as_f32() * scale));
            let _ = window.paint_glyph(origin, run.font_id, g.id, px(size), color);
        }
    }
}

// ---- root view: tiles, input, service ----------------------------------------

struct Fonts {
    mono: [Font; 4],
    ui: Font,
    advance: f32,
}

struct Spike {
    focus: FocusHandle,
    canvas: Entity<CanvasView>,
    cached: bool,
    fonts: Option<Fonts>,
    mode: Layout,
    tiles: Vec<String>,
    terms: HashMap<String, TermState>,
    focused_tile: usize,
    preedit: Option<String>,
    drag: Option<Point<Pixels>>,
    svc: Option<Arc<Service>>,
    driver: SharedDriver,
    last_geoms: Option<(Rect, [TileGeom; 4])>,
    hwnd: isize,
    out: Option<String>,
    demo: Option<&'static str>,
}

impl Spike {
    fn new(window: &mut Window, cx: &mut Context<Self>, a: &Args, driver: SharedDriver) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let hwnd = match raw_window_handle::HasWindowHandle::window_handle(window).map(|h| h.as_raw()) {
            Ok(raw_window_handle::RawWindowHandle::Win32(h)) => h.hwnd.get(),
            _ => 0,
        };
        let mut this = Spike {
            focus,
            canvas: cx.new(|_| CanvasView::new()),
            cached: !a.uncached,
            fonts: None,
            mode: Layout::Normal,
            tiles: Vec::new(),
            terms: HashMap::new(),
            focused_tile: 0,
            preedit: None,
            drag: None,
            svc: None,
            driver,
            last_geoms: None,
            hwnd,
            out: a.out.clone(),
            demo: if a.ime_demo { Some("ime") } else if a.altgr_demo { Some("altgr") } else { None },
        };
        this.connect(cx);
        if this.driver.is_some() {
            this.start_driver(cx);
        }
        if this.demo.is_some() {
            this.start_demo(cx);
        }
        this
    }

    fn set_status(&mut self, status: String, cx: &mut Context<Self>) {
        self.canvas.update(cx, |c, cx| {
            c.status = status;
            cx.notify();
        });
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<SvcEvent>();
        let status = match Service::connect(move |ev| {
            let _ = tx.unbounded_send(ev);
        }) {
            Ok(svc) => {
                let status = match svc.launch_all() {
                    Ok(()) => format!("service {}", svc.build),
                    Err(e) => format!("launch failed: {e:#}"),
                };
                self.tiles = vec![svc.ids.shell[0].clone(), svc.ids.noise[1].clone(), svc.ids.noise[2].clone(), svc.ids.noise[3].clone()];
                svc.subscribe(&self.tiles);
                self.svc = Some(Arc::new(svc));
                status
            }
            Err(e) => format!("{e:#}"),
        };
        self.set_status(status, cx);
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.next().await {
                let mut batch = vec![ev];
                while let Ok(ev) = rx.try_recv() {
                    batch.push(ev);
                }
                if this.update(cx, |view, cx| view.on_service(batch, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn on_service(&mut self, batch: Vec<SvcEvent>, cx: &mut Context<Self>) {
        let mut dirty = false;
        for ev in batch {
            match ev {
                SvcEvent::Frame(frame) => {
                    if let Some(d) = &self.driver {
                        d.lock().unwrap().service_frame(&frame);
                    }
                    dirty |= self.tiles.contains(&frame.id);
                    self.terms.entry(frame.id.clone()).or_default().apply(frame);
                }
                SvcEvent::Error(e) => self.set_status(e, cx),
                SvcEvent::Closed => self.set_status("service connection closed".into(), cx),
                SvcEvent::Terminals(_) => {}
            }
        }
        if let (Some(d), Some(svc)) = (&self.driver, &self.svc) {
            let mut d = d.lock().unwrap();
            if !d.is_ready() && svc.ids.noise.iter().all(|id| self.terms.get(id).is_some_and(|t| t.frames > 0)) {
                d.set_ready();
            }
        }
        if dirty {
            cx.notify();
        }
    }

    fn start_driver(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                let Ok(wait) = this.update(cx, |view, cx| view.drive(cx)) else { break };
                cx.background_executor().timer(wait).await;
            }
        })
        .detach();
    }

    fn drive(&mut self, cx: &mut Context<Self>) -> Duration {
        let Some(driver) = self.driver.clone() else { return Duration::from_secs(3600) };
        let now = Instant::now();
        let (next, actions) = {
            let mut d = driver.lock().unwrap();
            let next = d.tick(now);
            (next, d.take_actions())
        };
        for action in actions {
            match action {
                Action::Tiles(ids) => {
                    self.tiles = ids;
                    if let Some(svc) = &self.svc {
                        svc.subscribe(&self.tiles);
                    }
                }
                Action::Layout(mode) => self.mode = mode,
                Action::Animate(on) => self.canvas.update(cx, |c, cx| {
                    c.animate_from = on.then(Instant::now);
                    if !on {
                        c.camera = scene::initial_camera();
                    }
                    cx.notify();
                }),
                Action::Key(c) => {
                    self.focused_tile = 0;
                    keys::post_key(self.hwnd, c);
                }
                Action::Done(report) => {
                    println!("{report}");
                    if let Some(out) = &self.out {
                        let _ = std::fs::write(out, &report);
                    }
                    if let Some(svc) = &self.svc {
                        svc.stop_all();
                    }
                    std::thread::sleep(Duration::from_millis(300));
                    cx.quit();
                    return Duration::from_secs(3600);
                }
            }
            cx.notify();
        }
        next.saturating_duration_since(Instant::now()).max(Duration::from_millis(1))
    }

    /// `--ime-demo` / `--altgr-demo`: synthesized input on the focused tile.
    fn start_demo(&mut self, cx: &mut Context<Self>) {
        let kind = self.demo;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(4)).await;
            match kind {
                Some("ime") => {
                    // What GPUI's WM_IME_COMPOSITION handler calls: preedit, then commit.
                    let _ = this.update(cx, |v, cx| v.set_preedit("にほんご".into(), cx));
                    cx.background_executor().timer(Duration::from_secs(3)).await;
                    let _ = this.update(cx, |v, cx| v.commit_text("echo 日本語 ✓ 🙂", cx));
                }
                _ => {
                    // "echo " typed normally, then the WM_CHARs TranslateMessage posts
                    // for AltGr+Q/E/7/0 on a German layout (GPUI marks those key-downs
                    // prefer_character_input, so the text arrives through WM_CHAR).
                    let _ = this.update(cx, |v, _| {
                        for c in "echo ".chars() {
                            keys::post_key(v.hwnd, c);
                        }
                    });
                    cx.background_executor().timer(Duration::from_millis(400)).await;
                    let _ = this.update(cx, |v, _| {
                        for c in "@€{}".chars() {
                            keys::post_char(v.hwnd, c);
                        }
                    });
                }
            }
        })
        .detach();
    }

    fn set_preedit(&mut self, text: String, cx: &mut Context<Self>) {
        self.preedit = (!text.is_empty()).then_some(text);
        cx.notify();
    }

    fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.preedit = None;
        if let Some(input) = keys::text_input(text) {
            self.send_input(input);
        }
        cx.notify();
    }

    /// Typed input for the focused tile's terminal.
    fn send_input(&self, input: Input) {
        if let (Some(svc), Some(id)) = (&self.svc, self.tiles.get(self.focused_tile)) {
            if let Some(d) = &self.driver {
                d.lock().unwrap().forwarded();
            }
            svc.input(id, input);
        }
    }

    fn fonts(&mut self, window: &Window, cx: &App) -> &Fonts {
        if self.fonts.is_none() {
            let names = cx.text_system().all_font_names();
            let family = if names.iter().any(|n| n == "Cascadia Mono") { "Cascadia Mono" } else { "Consolas" };
            let base = font(family.to_owned());
            let mono = [
                base.clone(),
                Font { weight: FontWeight::BOLD, ..base.clone() },
                Font { style: FontStyle::Italic, ..base.clone() },
                Font { weight: FontWeight::BOLD, style: FontStyle::Italic, ..base.clone() },
            ];
            let ts = window.text_system();
            let id = ts.resolve_font(&mono[0]);
            let advance = ts.advance(id, px(100.0), 'M').map(|s| s.width.as_f32() / 100.0).unwrap_or(0.6);
            if let Some(d) = &self.driver {
                d.lock().unwrap().info("mono_font", serde_json::json!(family));
            }
            self.fonts = Some(Fonts { mono, ui: font("Segoe UI"), advance });
        }
        self.fonts.as_ref().unwrap()
    }

    // ---- input -------------------------------------------------------------

    fn key_down(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        match ks.key.as_str() {
            "f2" => {
                self.mode = if self.mode == Layout::Normal { Layout::TilesOnly } else { Layout::Normal };
                cx.stop_propagation();
                cx.notify();
                return;
            }
            "f3" => {
                self.focused_tile = (self.focused_tile + 1) % 4;
                cx.stop_propagation();
                cx.notify();
                return;
            }
            _ => {}
        }
        if self.preedit.is_some() {
            return;
        }
        let mods = keys::Mods { shift: ks.modifiers.shift, ctrl: ks.modifiers.control, alt: ks.modifiers.alt };
        let named = keys::named_from_str(&ks.key);
        // Plain text (and AltGr text) arrives through WM_CHAR → the input handler.
        if named.is_none() && (ev.prefer_character_input || !(mods.ctrl || mods.alt)) {
            return;
        }
        let base = if ks.key == "space" { Some(' ') } else { ks.key.chars().next().filter(|_| ks.key.chars().count() == 1) };
        if let Some(input) = keys::key_input(named, ks.key_char.as_deref(), base, mods) {
            self.send_input(input);
            cx.stop_propagation();
        }
    }

    fn mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let (x, y) = (ev.position.x.as_f32(), ev.position.y.as_f32());
        window.focus(&self.focus, cx);
        if let Some((canvas_rect, geoms)) = &self.last_geoms {
            if let Some(i) = geoms.iter().position(|g| g.outer.contains(x, y)) {
                self.focused_tile = i;
                cx.notify();
                return;
            }
            if canvas_rect.contains(x, y) {
                self.drag = Some(ev.position);
            }
        }
    }

    fn mouse_move(&mut self, ev: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(last) = self.drag {
            if !ev.dragging() {
                self.drag = None;
                return;
            }
            let d = ev.position - last;
            self.canvas.update(cx, |c, cx| {
                c.camera.pan(d.x.as_f32(), d.y.as_f32());
                cx.notify();
            });
            self.drag = Some(ev.position);
        }
    }

    fn mouse_up(&mut self, _ev: &MouseUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        self.drag = None;
    }

    fn scroll(&mut self, ev: &ScrollEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some((canvas_rect, _)) = self.last_geoms else { return };
        let (x, y) = (ev.position.x.as_f32(), ev.position.y.as_f32());
        if !canvas_rect.contains(x, y) {
            return;
        }
        let d = ev.delta.pixel_delta(px(20.0));
        let lines = match ev.delta {
            ScrollDelta::Lines(p) => p.y,
            ScrollDelta::Pixels(p) => p.y.as_f32() / 50.0,
        };
        let ctrl = ev.modifiers.control;
        self.canvas.update(cx, |c, cx| {
            if ctrl {
                c.camera.zoom_at(&canvas_rect, (x, y), 1.15f32.powf(lines));
            } else {
                c.camera.pan(d.x.as_f32(), d.y.as_f32());
            }
            cx.notify();
        });
    }

    // ---- painting ----------------------------------------------------------

    fn paint_tiles(&mut self, b: Bounds<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let t0 = Instant::now();
        let win = Rect::new(b.origin.x.as_f32(), b.origin.y.as_f32(), b.size.width.as_f32(), b.size.height.as_f32());
        let advance = self.fonts(window, cx).advance;
        let (canvas_rect, geoms) = term::layout(win, self.mode, advance);
        self.last_geoms = Some((canvas_rect, geoms));
        let mut cells = 0;
        for (i, geom) in geoms.iter().enumerate() {
            cells += self.paint_tile(i, geom, window, cx);
        }
        // IME: the focused tile's grid is the text input area.
        if let Some(geom) = geoms.get(self.focused_tile) {
            let entity = cx.entity();
            window.handle_input(&self.focus, ElementInputHandler::new(bounds(&geom.grid), entity), cx);
        }
        if let Some(d) = &self.driver {
            // The canvas painted before this element; zero when its cached paint was reused.
            let (boxes, canvas_ms) = self.canvas.update(cx, |c, _| (c.boxes, std::mem::take(&mut c.paint_ms)));
            let boxes = if self.mode == Layout::Normal { boxes } else { 0 };
            d.lock().unwrap().drawn(boxes, cells, canvas_ms, ms(t0.elapsed()));
        }
    }

    fn paint_tile(&mut self, index: usize, geom: &TileGeom, window: &mut Window, cx: &mut Context<Self>) -> usize {
        let focused = index == self.focused_tile;
        let id = self.tiles.get(index).cloned().unwrap_or_default();
        let border = if focused { 0x4fa3ff } else { 0x3a3d48 };
        window.paint_quad(quad(bounds(&geom.outer), px(6.0), hsla(term::DEFAULT_BG), px(if focused { 2.0 } else { 1.0 }), hsla(border), BorderStyle::Solid));
        let fonts = self.fonts.as_ref().unwrap();
        let (mono, ui) = (fonts.mono.clone(), fonts.ui.clone());
        let title = format!("{} · {}×{}{}", id, term::COLS, term::ROWS, if focused { " · keyboard focus" } else { "" });
        let run = TextRun { len: title.len(), font: ui, color: hsla(scene::TEXT_DIM), background_color: None, underline: None, strikethrough: None };
        let line = window.text_system().shape_line(title.into(), px(12.0), &[run], None);
        let _ = line.paint(point(px(geom.outer.x + 8.0), px(geom.outer.y + 2.0)), px(16.0), TextAlign::Left, None, window, cx);
        let Some(state) = self.terms.get(&id) else { return 0 };
        let (cw, ch, fs) = (geom.cell_w, geom.cell_h, px(geom.font_px));
        let (vis_cols, vis_rows) = (geom.visible_cols(), geom.visible_rows());
        let g = geom.grid;
        let preedit = if focused { self.preedit.clone() } else { None };
        let mut cells = 0;
        let first = term::first_row(state.rows.len(), vis_rows as usize, state.view.cursor);
        window.with_content_mask(Some(ContentMask { bounds: bounds(&g) }), |window| {
            for (y, row) in state.rows.iter().skip(first).enumerate().take(vis_rows as usize) {
                let top = g.y + y as f32 * ch;
                for span in &row.bgs {
                    if span.col < vis_cols {
                        window.paint_quad(fill(bounds(&Rect::new(g.x + span.col as f32 * cw, top, span.cells as f32 * cw, ch)), hsla(span.color)));
                    }
                }
            }
            for (y, row) in state.rows.iter().skip(first).enumerate().take(vis_rows as usize) {
                let top = g.y + y as f32 * ch;
                for seg in &row.segs {
                    if seg.col >= vis_cols {
                        continue;
                    }
                    cells += seg.cells as usize;
                    let f = &mono[(seg.style.bold as usize) | ((seg.style.italic as usize) << 1)];
                    let run = TextRun { len: seg.text.len(), font: f.clone(), color: hsla(seg.style.fg), background_color: None, underline: None, strikethrough: None };
                    let ascii = seg.text.is_ascii();
                    let text = &seg.text;
                    let line = window.text_system().shape_line_by_hash(seg.hash, text.len(), fs, &[run], ascii.then_some(px(cw)), || text.clone().into());
                    let mut x = g.x + seg.col as f32 * cw;
                    if !ascii {
                        // Centre fallback glyphs in their 1 or 2 cells.
                        x += ((seg.cells as f32 * cw - line.width().as_f32()) / 2.0).max(0.0);
                    }
                    let _ = line.paint(point(px(x), px(top)), px(ch), TextAlign::Left, None, window, cx);
                }
                for (span, yy) in row.underlines.iter().map(|s| (s, top + ch - 2.0)).chain(row.strikes.iter().map(|s| (s, top + ch / 2.0))) {
                    window.paint_quad(fill(bounds(&Rect::new(g.x + span.col as f32 * cw, yy, span.cells as f32 * cw, 1.0)), hsla(span.color)));
                }
            }
            if let Some((cx_, cy_)) = state.view.cursor
                && cx_ < vis_cols
                && (cy_ as usize) >= first
                && (cy_ as usize) < first + vis_rows as usize
            {
                let r = Rect::new(g.x + cx_ as f32 * cw, g.y + (cy_ as usize - first) as f32 * ch, cw, ch);
                if let Some(text) = &preedit {
                    // Inline IME preedit at the cursor, underlined.
                    let width = text.chars().map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)).sum::<usize>() as f32 * cw;
                    let pr = Rect::new(r.x, r.y, width.max(cw), ch);
                    window.paint_quad(fill(bounds(&pr), hsla(0x2d3350)));
                    let run = TextRun { len: text.len(), font: mono[0].clone(), color: hsla(0xffffff), background_color: None, underline: None, strikethrough: None };
                    let line = window.text_system().shape_line(text.clone().into(), fs, &[run], None);
                    let _ = line.paint(point(px(r.x), px(r.y)), px(ch), TextAlign::Left, None, window, cx);
                    window.paint_quad(fill(bounds(&Rect::new(pr.x, pr.y + ch - 2.0, pr.w, 1.5)), hsla(0xffb02e)));
                } else if focused {
                    window.paint_quad(fill(bounds(&r), hsla_a(0x4fa3ff, 0.65)));
                } else {
                    window.paint_quad(quad(bounds(&r), px(0.0), gpui::transparent_black(), px(1.0), hsla(0x8b949e), BorderStyle::Solid));
                }
            }
        });
        cells
    }

    fn cursor_bounds(&self) -> Option<Bounds<Pixels>> {
        let (_, geoms) = self.last_geoms.as_ref()?;
        let geom = geoms.get(self.focused_tile)?;
        let state = self.terms.get(self.tiles.get(self.focused_tile)?)?;
        let (cx_, cy_) = state.view.cursor?;
        let first = term::first_row(state.rows.len(), geom.visible_rows() as usize, state.view.cursor);
        let row = (cy_ as usize).saturating_sub(first);
        Some(bounds(&Rect::new(geom.grid.x + cx_ as f32 * geom.cell_w, geom.grid.y + row as f32 * geom.cell_h, geom.cell_w, geom.cell_h)))
    }
}

impl Render for Spike {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let width = window.viewport_size().width.as_f32();
        let canvas_w = if self.mode == Layout::Normal { (width / 2.0).floor() } else { 0.0 };
        let mut root = div()
            .size_full()
            .relative()
            .bg(hsla(scene::BG))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_scroll(cx.listener(Self::scroll));
        if canvas_w > 0.0 {
            let view = div().absolute().top_0().left_0().w(px(canvas_w)).h_full();
            root = if self.cached {
                root.child(view.child(self.canvas.clone().cached(StyleRefinement::default().size_full())))
            } else {
                root.child(view.child(self.canvas.clone()))
            };
        }
        root.child(
            canvas(|_, _, _| {}, move |b, _, window, cx| entity.update(cx, |view, cx| view.paint_tiles(b, window, cx)))
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
        )
    }
}

impl EntityInputHandler for Spike {
    fn text_for_range(&mut self, _range: Range<usize>, _adjusted: &mut Option<Range<usize>>, _w: &mut Window, _cx: &mut Context<Self>) -> Option<String> {
        Some(self.preedit.clone().unwrap_or_default())
    }

    fn selected_text_range(&mut self, _ignore: bool, _w: &mut Window, _cx: &mut Context<Self>) -> Option<UTF16Selection> {
        let len = self.preedit.as_ref().map(|p| p.encode_utf16().count()).unwrap_or(0);
        Some(UTF16Selection { range: len..len, reversed: false })
    }

    fn marked_text_range(&self, _w: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
        self.preedit.as_ref().map(|p| 0..p.encode_utf16().count())
    }

    fn unmark_text(&mut self, _w: &mut Window, cx: &mut Context<Self>) {
        self.preedit = None;
        cx.notify();
    }

    fn replace_text_in_range(&mut self, _range: Option<Range<usize>>, text: &str, _w: &mut Window, cx: &mut Context<Self>) {
        self.commit_text(text, cx);
    }

    fn replace_and_mark_text_in_range(&mut self, _range: Option<Range<usize>>, text: &str, _sel: Option<Range<usize>>, _w: &mut Window, cx: &mut Context<Self>) {
        self.set_preedit(text.to_owned(), cx);
    }

    fn bounds_for_range(&mut self, _range: Range<usize>, _element: Bounds<Pixels>, _w: &mut Window, _cx: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        self.cursor_bounds()
    }

    fn character_index_for_point(&mut self, _p: Point<Pixels>, _w: &mut Window, _cx: &mut Context<Self>) -> Option<usize> {
        None
    }
}

/// GPUI reports each frame's draw + present time on stderr when
/// ZED_MEASUREMENTS=1 ("frame duration: 1.23ms"); `--measure` redirects this
/// process's stderr into a pipe and feeds those lines to the driver.
fn capture_frame_durations(driver: Arc<Mutex<Driver>>) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, SetStdHandle};
    let (reader, writer) = std::io::pipe()?;
    unsafe { SetStdHandle(STD_ERROR_HANDLE, writer.as_raw_handle() as _) };
    std::mem::forget(writer);
    let log_path = std::env::temp_dir().join("gpui-spike-stderr.log");
    std::thread::Builder::new().name("frame-durations".into()).spawn(move || {
        let mut log = std::fs::File::create(log_path).ok();
        for line in std::io::BufReader::new(reader).lines() {
            let Ok(line) = line else { break };
            let now = Instant::now();
            if let Some(ms) = line.strip_prefix("frame duration: ").and_then(parse_duration_ms) {
                driver.lock().unwrap().presented(ms, 0.0, now);
            } else if let Some(log) = log.as_mut() {
                use std::io::Write;
                let _ = writeln!(log, "{line}");
            }
        }
    })?;
    Ok(())
}

fn parse_duration_ms(text: &str) -> Option<f64> {
    let text = text.trim();
    for (suffix, scale) in [("ms", 1.0), ("µs", 1e-3), ("us", 1e-3), ("ns", 1e-6), ("s", 1e3)] {
        if let Some(number) = text.strip_suffix(suffix) {
            return number.parse::<f64>().ok().map(|v| v * scale);
        }
    }
    None
}

fn main() {
    let a = args();
    if a.warp {
        // Honoured only by the WARP-patched gpui-pre-windows build (warp/build-warp.ps1).
        unsafe { std::env::set_var("GPUI_FORCE_WARP", "1") };
    }
    let driver = a.measure.then(|| {
        unsafe { std::env::set_var("ZED_MEASUREMENTS", "1") };
        let ids = service::Ids::new();
        let driver = Arc::new(Mutex::new(Driver::new("gpui", measure::Ids { noise: ids.noise.to_vec(), shell: ids.shell.to_vec(), echo: ids.echo })));
        driver.lock().unwrap().memory_checkpoint("at_start");
        if let Err(e) = capture_frame_durations(driver.clone()) {
            println!("{{\"error\": \"cannot capture frame durations: {e}\"}}");
        }
        driver
    });
    gpui_platform::application().run(move |cx: &mut App| {
        let window_bounds = Bounds::centered(None, size(px(1800.0), px(1000.0)), cx);
        let options = WindowOptions { window_bounds: Some(WindowBounds::Windowed(window_bounds)), focus: true, ..Default::default() };
        let driver = driver.clone();
        let cached = !a.uncached;
        let handle = cx
            .open_window(options, move |window, cx| {
                window.set_window_title("Copilot Canvas — GPUI spike");
                cx.new(|cx| Spike::new(window, cx, &a, driver))
            })
            .expect("open window");
        let _ = handle.update(cx, |view, window, _cx| {
            if let Some(d) = &view.driver {
                let mut d = d.lock().unwrap();
                d.memory_checkpoint("after_gpu_init");
                let specs = window.gpu_specs();
                d.info("adapter", serde_json::json!(specs.as_ref().map(|s| s.device_name.clone())));
                d.info("software_renderer", serde_json::json!(specs.as_ref().map(|s| s.is_software_emulated)));
                d.info("scale_factor", serde_json::json!(window.scale_factor()));
                d.info("window_logical", serde_json::json!([1800, 1000]));
                d.info("gpui_force_warp_env", serde_json::json!(std::env::var("GPUI_FORCE_WARP").is_ok()));
                d.info("canvas_view_cached", serde_json::json!(cached));
            }
        });
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        cx.activate(true);
    });
}
