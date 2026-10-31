//! The report's HTML view on Linux: WebKitGTK rendered offscreen in a helper
//! process (`html_helper`), its frames drawn by the GUI as a texture over the
//! report pane, the window's input over the pane forwarded to it
//! (MAC-LINUX-PLAN §5.7; PORT-STATUS records the spike and the decision).
//!
//! The helper starts the first time an HTML report is shown and lives as
//! long as this window. When WebKitGTK is missing or the helper fails, the
//! core shows reports as formatted text and says why, as on Windows without
//! the WebView2 runtime. The view never takes the keyboard by itself: a click
//! into it does, and Esc then closes the report.

use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};

use canvas_core::view::HtmlPane;
use crossbeam_channel::Sender;
use serde_json::{Value, json};

use super::html_helper::{ENV, HOME_ENV};

/// What happened in the view that the window acts on.
pub enum WebEvent {
    Ready,
    Failed(String),
    /// The user clicked a link (the canvas asks before opening it).
    Link(String),
    /// Esc in the view: the report closes.
    Escape,
}

#[derive(Clone, Debug, Default, PartialEq)]
enum Phase {
    #[default]
    Idle,
    Starting,
    Ready,
    Failed(String),
}

/// A frame's texture key: unique per frame, apart from other images' keys.
const FRAME_KEYS: u64 = 0xC0DE_0000_0000_0000;

#[derive(Default)]
struct Shared {
    phase: Phase,
    events: Vec<WebEvent>,
    frame: Option<(u64, u32, u32, Arc<Vec<u8>>)>,
    frames: u64,
    loads: u32,
    title: String,
    source: String,
    webkit: String,
    sandbox: bool,
    blocked_navigations: Vec<String>,
    blocked_requests: u32,
    blocked_request_lines: Vec<String>,
    blocked_windows: u32,
    blocked_downloads: u32,
    denied_permissions: u32,
    /// The last text copied from the view.
    copied: String,
    replies: HashMap<u64, Sender<Value>>,
}

fn lock(shared: &Mutex<Shared>) -> std::sync::MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|p| p.into_inner())
}

fn push_capped(list: &mut Vec<String>, item: String) {
    list.push(item.chars().take(160).collect());
    if list.len() > 12 {
        list.remove(0);
    }
}

/// The helper process and the pipe to it.
struct Helper {
    child: Child,
    stdin: ChildStdin,
}

/// One window's HTML view.
pub struct ReportWeb {
    home: std::path::PathBuf,
    shared: Arc<Mutex<Shared>>,
    helper: Option<Helper>,
    /// The report whose page is loaded.
    shown: Option<String>,
    visible: bool,
    /// The pane in window pixels: x, y, w, h.
    rect: Option<[f32; 4]>,
    zoom: f32,
    /// A click went into the view: keys go to it until a click elsewhere.
    focused: bool,
    held: bool,
    next_eval: u64,
}

impl ReportWeb {
    pub fn new(home: &std::path::Path) -> Self {
        ReportWeb { home: home.to_path_buf(), shared: Arc::default(), helper: None, shown: None, visible: false, rect: None, zoom: 1.0, focused: false, held: false, next_eval: 0 }
    }

    pub fn take_events(&mut self) -> Vec<WebEvent> {
        std::mem::take(&mut lock(&self.shared).events)
    }

    /// The latest frame over the pane, while a report shows.
    pub fn frame(&self) -> Option<crate::scene::ImageItem> {
        let rect = self.rect.filter(|_| self.visible)?;
        let (key, width, height, rgba) = lock(&self.shared).frame.clone()?;
        // Drawn at its own size from the pane's corner (a frame of the old
        // size until the helper catches up with a resize), clipped to the pane.
        let rect_px = [rect[0], rect[1], width as f32, height as f32];
        Some(crate::scene::ImageItem { key, width, height, rgba, rect: rect_px, uv: [0.0, 0.0, 1.0, 1.0], clip: [rect[0], rect[1], rect[0] + rect[2], rect[1] + rect[3]] })
    }

    fn send(&mut self, command: Value) {
        let Some(helper) = self.helper.as_mut() else { return };
        if writeln!(helper.stdin, "{command}").and_then(|_| helper.stdin.flush()).is_err() {
            fail(&self.shared, "the HTML view's helper stopped".into());
        }
    }

    fn inside(&self, x: f32, y: f32) -> Option<(f32, f32)> {
        let r = self.rect.filter(|_| self.visible && lock(&self.shared).phase == Phase::Ready)?;
        (x >= r[0] && y >= r[1] && x < r[0] + r[2] && y < r[1] + r[3]).then_some((x - r[0], y - r[1]))
    }

    /// The window's input while a report shows: wheel, pointer and buttons
    /// over the pane go to the view; keys after a click into it (Esc closes
    /// the report).
    pub fn input(&mut self, event: &crate::os::WebInput) -> bool {
        use crate::os::WebInput;
        use winit::event::{MouseButton, MouseScrollDelta, TouchPhase};
        match event {
            WebInput::Wheel { x, y, delta, phase } => {
                let Some((x, y)) = self.inside(*x, *y) else { return false };
                // GDK's smooth deltas: one step per line; pixels as WebKit's
                // 40-pixel line step (touchpads, with the gesture's end).
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(dx, dy) => (-*dx as f64, -*dy as f64),
                    MouseScrollDelta::PixelDelta(p) => (-p.x / 40.0, -p.y / 40.0),
                };
                let stop = matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled);
                self.send(json!({ "cmd": "wheel", "x": x, "y": y, "dx": dx, "dy": dy, "stop": stop }));
                true
            }
            WebInput::Motion { x, y } => {
                let Some((x, y)) = self.inside(*x, *y) else { return false };
                let held = self.held;
                self.send(json!({ "cmd": "motion", "x": x, "y": y, "held": held }));
                true
            }
            WebInput::Button { x, y, button, pressed } => {
                let Some((x, y)) = self.inside(*x, *y).filter(|_| *pressed || self.held) else {
                    if *pressed {
                        self.focused = false;
                    }
                    return false;
                };
                let number = match button {
                    MouseButton::Left => 1,
                    MouseButton::Middle => 2,
                    MouseButton::Right => 3,
                    _ => return true,
                };
                if number == 1 {
                    self.held = *pressed;
                }
                self.focused = true;
                self.send(json!({ "cmd": "button", "x": x, "y": y, "button": number, "pressed": pressed }));
                true
            }
            WebInput::Key { key, pressed, mods } => {
                if !self.focused || !self.visible {
                    return false;
                }
                if matches!(key, winit::keyboard::Key::Named(winit::keyboard::NamedKey::Escape)) {
                    if *pressed {
                        self.focused = false;
                        lock(&self.shared).events.push(WebEvent::Escape);
                        crate::shell::wake_web();
                    }
                    return true;
                }
                // Copy: the page's selection to the clipboard (the helper reads it).
                let copy = mods.control_key() && matches!(key, winit::keyboard::Key::Character(c) if c.eq_ignore_ascii_case("c"));
                if copy {
                    if *pressed {
                        self.send(json!({ "cmd": "copy" }));
                    }
                    return true;
                }
                let Some(keyval) = keyval(key) else { return true };
                self.send(json!({ "cmd": "key", "keyval": keyval, "pressed": pressed, "ctrl": mods.control_key(), "shift": mods.shift_key() }));
                true
            }
        }
    }

    /// Shows `pane` (an HTML report) over its area, or hides the view; the
    /// helper starts the first time one is shown.
    pub fn sync(&mut self, _window: isize, pane: Option<&HtmlPane>, cell_w: f32, cell_h: f32) {
        let Some(pane) = pane else {
            self.visible = false;
            self.focused = false;
            return;
        };
        let phase = lock(&self.shared).phase.clone();
        match phase {
            Phase::Idle => self.start(),
            Phase::Starting | Phase::Failed(_) => {}
            Phase::Ready => {
                let r = pane.rect;
                let rect = [r.x as f32 * cell_w, r.y as f32 * cell_h, r.width as f32 * cell_w, r.height as f32 * cell_h];
                // The page's CSS pixels follow the window's scale (cells are 18.2 px at 100 %).
                let zoom = ((cell_h / 18.2) * 4.0).round().max(4.0) / 4.0;
                if self.rect.map(|o| (o[2], o[3])) != Some((rect[2], rect[3])) || self.zoom != zoom {
                    self.send(json!({ "cmd": "size", "w": rect[2].round(), "h": rect[3].round(), "zoom": zoom }));
                }
                self.rect = Some(rect);
                self.zoom = zoom;
                if self.shown.as_deref() != Some(pane.id.as_str()) {
                    self.send(json!({ "cmd": "load", "html": pane.document.as_str() }));
                    self.shown = Some(pane.id.clone());
                }
                self.visible = true;
            }
        }
    }

    fn start(&mut self) {
        lock(&self.shared).phase = Phase::Starting;
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(error) => return fail(&self.shared, format!("the HTML view could not start: {error}")),
        };
        let spawned = Command::new(exe)
            .env(ENV, "1")
            .env(HOME_ENV, &self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => return fail(&self.shared, format!("the HTML view could not start: {error}")),
        };
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else { return fail(&self.shared, "the HTML view has no pipes".into()) };
        let shared = self.shared.clone();
        let _ = std::thread::Builder::new().name("html-view".into()).spawn(move || read(BufReader::new(stdout), &shared));
        self.helper = Some(Helper { child, stdin });
    }

    /// The view's state for the test hooks.
    pub fn json(&self) -> Value {
        let s = lock(&self.shared);
        json!({
            "ok": true,
            "phase": match &s.phase { Phase::Idle => "idle".to_owned(), Phase::Starting => "creating".into(), Phase::Ready => "ready".into(), Phase::Failed(why) => format!("failed: {why}") },
            "engine": format!("WebKitGTK {} (offscreen{})", s.webkit, if s.sandbox { ", sandboxed" } else { "" }),
            "visible": self.visible,
            "bounds": self.rect.map(|r| [r[0].round() as i32, r[1].round() as i32, (r[0] + r[2]).round() as i32, (r[1] + r[3]).round() as i32]),
            "report": self.shown,
            "title": s.title,
            "source": s.source,
            "loads": s.loads,
            "frames": s.frames,
            "focused": self.focused,
            "blocked_navigations": s.blocked_navigations,
            "blocked_requests": s.blocked_requests,
            "blocked_request_uris": s.blocked_request_lines,
            "blocked_windows": s.blocked_windows,
            "blocked_downloads": s.blocked_downloads,
            "denied_permissions": s.denied_permissions,
            "copied": s.copied,
        })
    }

    /// Runs script in the page as the host (test hooks only: the page's own
    /// scripts never run) and answers its result through `reply`.
    pub fn eval(&mut self, script: &str, reply: Sender<Value>) {
        if self.helper.is_none() || lock(&self.shared).phase != Phase::Ready {
            let _ = reply.send(json!({ "ok": false, "error": "no HTML view" }));
            return;
        }
        self.next_eval += 1;
        let id = self.next_eval;
        lock(&self.shared).replies.insert(id, reply);
        self.send(json!({ "cmd": "eval", "id": id, "script": script }));
    }

    /// Loads HTML as it is, without the canvas's sanitizer (test hooks only:
    /// checks that the view's own lock-down holds by itself).
    pub fn probe_raw(&mut self, html: &str) -> Value {
        if self.helper.is_none() {
            return json!({ "ok": false, "error": "no HTML view" });
        }
        self.send(json!({ "cmd": "load", "html": html }));
        json!({ "ok": true })
    }

    /// What the view shows as a PNG file (its latest frame).
    pub fn capture(&self, path: std::path::PathBuf, reply: Sender<Value>) {
        let frame = lock(&self.shared).frame.clone();
        let _ = reply.send(match frame {
            None => json!({ "ok": false, "error": "no frame yet" }),
            Some((_, w, h, rgba)) => match crate::gpu::write_png(&path, w, h, &rgba) {
                Ok(()) => json!({ "ok": true, "path": path, "width": w, "height": h }),
                Err(error) => json!({ "ok": false, "error": format!("{error:#}") }),
            },
        });
    }
}

impl Drop for ReportWeb {
    fn drop(&mut self) {
        if let Some(mut helper) = self.helper.take() {
            let _ = helper.child.kill();
            let _ = helper.child.wait();
        }
    }
}

/// The composed window is the GUI's own frame here (the view is part of it):
/// the shell renders it offscreen instead.
pub fn capture_window(_window: isize, _path: &std::path::Path) -> anyhow::Result<(u32, u32)> {
    anyhow::bail!("the HTML view is drawn by the GUI: capture its own frame")
}

fn fail(shared: &Mutex<Shared>, why: String) {
    {
        let mut s = lock(shared);
        if matches!(s.phase, Phase::Failed(_)) {
            return;
        }
        s.phase = Phase::Failed(why.clone());
        s.events.push(WebEvent::Failed(why));
        for (_, reply) in s.replies.drain() {
            let _ = reply.send(json!({ "ok": false, "error": "the HTML view stopped" }));
        }
    }
    crate::shell::wake_web();
}

/// The helper's records: events and frames.
fn read(mut out: impl Read, shared: &Mutex<Shared>) {
    let mut head = [0u8; 5];
    while out.read_exact(&mut head).is_ok() {
        let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
        let mut body = vec![0u8; len];
        if out.read_exact(&mut body).is_err() {
            break;
        }
        match head[0] {
            b'F' if len >= 8 => {
                let (w, h) = (u32::from_le_bytes(body[0..4].try_into().unwrap_or_default()), u32::from_le_bytes(body[4..8].try_into().unwrap_or_default()));
                let rgba = body.split_off(8);
                if rgba.len() == (w * h * 4) as usize {
                    let mut s = lock(shared);
                    s.frames += 1;
                    let key = FRAME_KEYS | s.frames;
                    s.frame = Some((key, w, h, Arc::new(rgba)));
                }
            }
            b'J' => {
                let Ok(value) = serde_json::from_slice::<Value>(&body) else { continue };
                apply(shared, &value);
            }
            _ => {}
        }
        crate::shell::wake_web();
    }
    fail(shared, "the HTML view's helper ended (WebKitGTK missing or failed; see the GUI's log)".into());
}

fn apply(shared: &Mutex<Shared>, value: &Value) {
    let text = |k: &str| value[k].as_str().unwrap_or_default().to_owned();
    match value["event"].as_str().unwrap_or_default() {
        "ready" => {
            let mut s = lock(shared);
            s.phase = Phase::Ready;
            s.webkit = text("webkit");
            s.sandbox = value["sandbox"].as_bool().unwrap_or(false);
            s.events.push(WebEvent::Ready);
        }
        "failed" => fail(shared, text("why")),
        "loaded" => {
            let mut s = lock(shared);
            s.loads = value["loads"].as_u64().unwrap_or(0) as u32;
            s.title = text("title");
            s.source = text("source");
        }
        "link" => lock(shared).events.push(WebEvent::Link(text("url"))),
        "blocked_navigation" => push_capped(&mut lock(shared).blocked_navigations, text("url")),
        "blocked_request" => {
            let mut s = lock(shared);
            s.blocked_requests += 1;
            push_capped(&mut s.blocked_request_lines, text("request"));
        }
        "copied" => {
            let text = text("text");
            if !text.is_empty() {
                let _ = canvas_sys::clipboard::set_text(&text);
            }
            lock(shared).copied = text;
        }
        "blocked_window" => lock(shared).blocked_windows += 1,
        "blocked_download" => lock(shared).blocked_downloads += 1,
        "denied_permission" => lock(shared).denied_permissions += 1,
        "reply" => {
            let id = value["id"].as_u64().unwrap_or(0);
            if let Some(reply) = lock(shared).replies.remove(&id) {
                let answer = if value["ok"] == true { json!({ "ok": true, "result": value["result"] }) } else { json!({ "ok": false, "error": value["error"] }) };
                let _ = reply.send(answer);
            }
        }
        _ => {}
    }
}

/// The GDK key value of a key the view handles (navigation, editing keys and
/// characters for copy, select all and find).
fn keyval(key: &winit::keyboard::Key) -> Option<u32> {
    use winit::keyboard::{Key, NamedKey};
    Some(match key {
        Key::Named(NamedKey::ArrowUp) => 0xff52,
        Key::Named(NamedKey::ArrowDown) => 0xff54,
        Key::Named(NamedKey::ArrowLeft) => 0xff51,
        Key::Named(NamedKey::ArrowRight) => 0xff53,
        Key::Named(NamedKey::PageUp) => 0xff55,
        Key::Named(NamedKey::PageDown) => 0xff56,
        Key::Named(NamedKey::Home) => 0xff50,
        Key::Named(NamedKey::End) => 0xff57,
        Key::Named(NamedKey::Tab) => 0xff09,
        Key::Named(NamedKey::Enter) => 0xff0d,
        Key::Named(NamedKey::Space) => 0x20,
        Key::Character(text) => {
            let mut chars = text.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            let code = c as u32;
            if (0x20..0x7f).contains(&code) || (0xa0..0x100).contains(&code) { code } else { 0x0100_0000 | code }
        }
        _ => return None,
    })
}
