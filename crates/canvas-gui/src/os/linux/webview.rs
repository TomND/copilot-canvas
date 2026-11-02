//! The HTML view on Linux (reports, rendered Markdown notes, HTML notes):
//! WebKitGTK rendered offscreen in a helper process (`html_helper`), its
//! frames drawn by the GUI as a texture over the pane, the window's input
//! over the pane forwarded to it (MAC-LINUX-PLAN §5.7; NOTEBOOK-TASKS §4).
//!
//! As WebView2 on Windows: two views (the plain one never runs a page's
//! scripts; HTML notes allowed to run theirs get the scripts view, in its
//! own ephemeral context, unloaded whenever it is hidden). HTML notes are
//! served from `canvas-notes://notes.canvas.invalid/` (WebKit lets no handler
//! serve https), mapped to the note's explorer root: every request of that
//! scheme comes here, is put in the notes origin's form
//! (`https://notes.canvas.invalid/…`) and decided by `webpane::classify`
//! (only the note's own folder and the folders below it, `GET` only); files
//! are read through the backend on reader threads (the service's machine,
//! also remotely; at most `webpane::MAX_PENDING` waiting, the rest 503) and
//! answered with the note's policy (`webpane::reply`, its origin written as
//! the view's). Everything else is 403. Links go through `webpane`: http(s)
//! and mailto ask first, `#note:` / `#todo:` and other files under the root
//! navigate the app (in the scripts view once per click into it), global
//! keys (`webpane::web_key`) go to the canvas, *Print / PDF* saves the page
//! as `<Downloads>/<title>.pdf` (`-2`, `-3`: never over a file).
//!
//! The helper starts the first time a pane is shown and lives as long as
//! this window. When WebKitGTK is missing or the helper fails, the core shows
//! pages as formatted text and says why, as on Windows without the WebView2
//! runtime. The view never takes the keyboard by itself: a click into it
//! does; Esc then closes a report or gives the keyboard back to the canvas.

use std::collections::HashMap;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use canvas_core::input::KeyEvent;
use canvas_core::rpc::Backend;
use canvas_core::spaces::{NavTarget, Space};
use canvas_core::view::{HtmlPane, HtmlSource};
use canvas_model::report_html::notes;
use crossbeam_channel::Sender;
use serde_json::{Value, json};

use super::html_helper::{ENV, HOME_ENV, Role, SCHEME, read_record, write_record};
use crate::webpane::{self, AppLinks, LinkAction, Reply, Resource, Serving, WebKey};

/// Threads reading notes' files through the backend.
const READERS: usize = 4;

/// What happened in the view that the window acts on.
pub enum WebEvent {
    Ready,
    Failed(String),
    /// The user clicked an http(s)/mailto link (the canvas asks before opening it).
    Link(String),
    /// Esc in the view: a report closes; else the keyboard goes back to the canvas.
    Escape,
    /// A global key pressed in the page, for the canvas.
    Key(KeyEvent),
    /// A `#note:` / `#todo:` link or a link to another file of the note's root.
    Navigate(Space, NavTarget),
    /// Something to tell the user (printing); `true`: an error.
    Notice(String, bool),
}

#[derive(Clone, Debug, Default, PartialEq)]
enum Phase {
    #[default]
    Idle,
    Starting,
    Ready,
    Failed(String),
}

fn phase_text(phase: &Phase) -> String {
    match phase {
        Phase::Idle => "idle".to_owned(),
        Phase::Starting => "creating".into(),
        Phase::Ready => "ready".into(),
        Phase::Failed(why) => format!("failed: {why}"),
    }
}

/// A frame's texture key: unique per frame, apart from other images' keys.
const FRAME_KEYS: u64 = 0xC0DE_0000_0000_0000;

/// The notes origin as the view loads it: WebKit serves no https of its own,
/// so `https://notes.canvas.invalid/…` is `canvas-notes://notes.canvas.invalid/…` there.
fn view_origin() -> String {
    format!("{SCHEME}://{}", notes::HOST)
}

/// A URL of the notes origin as the view loads it (others as they are).
pub fn to_view(url: &str) -> String {
    match url.get(..notes::ORIGIN.len()) {
        Some(head) if head.eq_ignore_ascii_case(notes::ORIGIN) => format!("{}{}", view_origin(), &url[notes::ORIGIN.len()..]),
        _ => url.to_owned(),
    }
}

/// A URL the view reports, in the notes origin's form (`webpane` decides on it).
pub fn to_origin(uri: &str) -> String {
    let origin = view_origin();
    match uri.get(..origin.len()) {
        Some(head) if head.eq_ignore_ascii_case(&origin) => format!("{}{}", notes::ORIGIN, &uri[origin.len()..]),
        _ => uri.to_owned(),
    }
}

/// A reply as the view serves it: its policy names the view's origin, and so
/// does a sanitized page (its policy and its links to the origin's files).
pub fn for_view(mut reply: Reply, main: bool, scripts: bool) -> Reply {
    let origin = view_origin();
    reply.headers = reply.headers.replace(notes::ORIGIN, &origin);
    if main && !scripts && reply.headers.contains("text/html") {
        reply.body = match String::from_utf8(std::mem::take(&mut reply.body)) {
            Ok(page) => page.replace(notes::ORIGIN, &origin).into_bytes(),
            Err(bytes) => bytes.into_bytes(),
        };
    }
    reply
}

/// What a view shows: the pane's id and, for an HTML note, what it serves.
type Shown = (String, Option<Serving>);

/// One of the two views, as the GUI knows it.
#[derive(Default)]
struct Slot {
    /// The plain view comes with the helper; the scripts view when first needed.
    phase: Phase,
    shown: Option<Shown>,
    /// The HTML note served (its explorer root is the origin's root; its folder is served).
    serving: Option<Serving>,
    visible: bool,
    /// The page's load in flight (its number).
    loading: Option<u64>,
    links: AppLinks,
    /// Its page's number: requests of a page that was replaced are not read.
    generation: Arc<AtomicU64>,
    title: String,
    source: String,
    frame: Option<(u64, u32, u32, Arc<Vec<u8>>)>,
}

/// A file to read through the backend for request `id` (skipped unless
/// `live` is still `generation`: the page that asked is still shown).
struct Job {
    id: u64,
    path: String,
    main: bool,
    scripts: bool,
    backend: Arc<dyn Backend>,
    live: Arc<AtomicU64>,
    generation: u64,
}

#[derive(Default)]
struct Shared {
    phase: Phase,
    events: Vec<WebEvent>,
    slots: [Slot; 2],
    /// The view whose frames are drawn.
    active: Option<Role>,
    /// The backend files are read through (the window's, set every frame).
    backend: Option<Arc<dyn Backend>>,
    /// Requests of the notes scheme waiting for their file, by id.
    pending: HashMap<u64, Role>,
    jobs: Option<crossbeam_channel::Sender<Job>>,
    frames: u64,
    loads: u32,
    webkit: String,
    sandbox: bool,
    blocked_navigations: Vec<String>,
    blocked_requests: u32,
    blocked_request_uris: Vec<String>,
    served: u32,
    served_request_uris: Vec<String>,
    app_links: u32,
    blocked_windows: u32,
    blocked_downloads: u32,
    denied_permissions: u32,
    /// *Print / PDF* asked for, waiting for the page to finish loading.
    print: bool,
    /// The PDF being written, and the last one saved.
    printing: Option<PathBuf>,
    printed: Option<PathBuf>,
    /// The last text copied from the view.
    copied: String,
    replies: HashMap<u64, Sender<Value>>,
}

impl Shared {
    fn slot(&mut self, role: Role) -> &mut Slot {
        &mut self.slots[role as usize]
    }
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

fn wake() {
    crate::shell::wake_web();
}

/// The pipe to the helper: whole records, from any thread.
#[derive(Default)]
struct Pipe(Mutex<Option<ChildStdin>>);

impl Pipe {
    fn record(&self, kind: u8, body: &[u8]) -> bool {
        let mut stdin = self.0.lock().unwrap_or_else(|p| p.into_inner());
        stdin.as_mut().is_some_and(|stdin| write_record(stdin, kind, body).is_ok())
    }

    fn command(&self, command: &Value) -> bool {
        self.record(b'J', command.to_string().as_bytes())
    }

    /// Answers request `id` of the notes scheme.
    fn reply(&self, id: u64, reply: &Reply) -> bool {
        let head = json!({ "id": id, "status": reply.status, "reason": reply.reason, "headers": reply.headers }).to_string();
        let mut body = (head.len() as u32).to_le_bytes().to_vec();
        body.extend(head.as_bytes());
        body.extend(&reply.body);
        self.record(b'R', &body)
    }
}

/// One window's HTML view.
pub struct ReportWeb {
    home: PathBuf,
    shared: Arc<Mutex<Shared>>,
    pipe: Arc<Pipe>,
    child: Option<Child>,
    /// The pane in window pixels: x, y, w, h.
    rect: Option<[f32; 4]>,
    zoom: f32,
    /// A click went into the view: keys go to it until a click elsewhere.
    focused: bool,
    held: bool,
    next_eval: u64,
    next_load: u64,
}

impl ReportWeb {
    pub fn new(home: &Path) -> Self {
        ReportWeb { home: home.to_path_buf(), shared: Arc::default(), pipe: Arc::default(), child: None, rect: None, zoom: 1.0, focused: false, held: false, next_eval: 0, next_load: 0 }
    }

    pub fn take_events(&mut self) -> Vec<WebEvent> {
        std::mem::take(&mut lock(&self.shared).events)
    }

    /// The view shown, if one is.
    fn shown_role(&self) -> Option<Role> {
        let s = lock(&self.shared);
        s.active.filter(|role| s.slots[*role as usize].visible)
    }

    /// The latest frame over the pane, while a page shows.
    pub fn frame(&self) -> Option<crate::scene::ImageItem> {
        let rect = self.rect?;
        let role = self.shown_role()?;
        let (key, width, height, rgba) = lock(&self.shared).slots[role as usize].frame.clone()?;
        // Drawn at its own size from the pane's corner (a frame of the old
        // size until the helper catches up with a resize), clipped to the pane.
        let rect_px = [rect[0], rect[1], width as f32, height as f32];
        Some(crate::scene::ImageItem { key, width, height, rgba, rect: rect_px, uv: [0.0, 0.0, 1.0, 1.0], clip: [rect[0], rect[1], rect[0] + rect[2], rect[1] + rect[3]] })
    }

    fn send(&self, command: Value) {
        if self.child.is_some() && !self.pipe.command(&command) {
            fail(&self.shared, "the HTML view's helper stopped".into());
        }
    }

    fn inside(&self, x: f32, y: f32) -> Option<(f32, f32)> {
        let r = self.rect?;
        self.shown_role()?;
        (lock(&self.shared).phase == Phase::Ready && x >= r[0] && y >= r[1] && x < r[0] + r[2] && y < r[1] + r[3]).then_some((x - r[0], y - r[1]))
    }

    fn focus(&mut self, got: bool) {
        self.focused = got;
        let role = self.shown_role();
        for (i, slot) in lock(&self.shared).slots.iter_mut().enumerate() {
            slot.links.focus(got && role.is_some_and(|r| r as usize == i));
        }
    }

    /// The window's input while a page shows: wheel, pointer and buttons over
    /// the pane go to the view; keys after a click into it, but the global
    /// ones (`webpane::web_key`) go to the canvas and Esc closes a report or
    /// gives the keyboard back.
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
                        self.focus(false);
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
                if *pressed {
                    self.focus(true);
                }
                self.send(json!({ "cmd": "button", "x": x, "y": y, "button": number, "pressed": pressed }));
                true
            }
            WebInput::Key { key, pressed, mods } => {
                if !self.focused || self.shown_role().is_none() {
                    return false;
                }
                if let Some(action) = web_key_of(key, *mods) {
                    if *pressed {
                        let event = match action {
                            WebKey::Escape => {
                                self.focus(false);
                                WebEvent::Escape
                            }
                            WebKey::Forward(key) => WebEvent::Key(key),
                        };
                        lock(&self.shared).events.push(event);
                        wake();
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

    /// Shows `pane` over its area, or hides the views (`None`). A whole page
    /// (`HtmlSource::Document`: reports, rendered Markdown) loads as a string
    /// in the plain view; an HTML note (`HtmlSource::File`) from the notes
    /// scheme, in the scripts view when its scripts may run (in the plain
    /// one, without them, if that view could not start). The helper starts
    /// the first time a pane is shown; `backend` reads the notes' files.
    pub fn sync(&mut self, _window: isize, pane: Option<&HtmlPane>, backend: &Arc<dyn Backend>, cell_w: f32, cell_h: f32) {
        lock(&self.shared).backend = Some(backend.clone());
        let Some(pane) = pane else {
            lock(&self.shared).print = false;
            self.hide(Role::Plain);
            self.hide(Role::Scripts);
            return;
        };
        let (role, page) = self.page_of(pane);
        self.hide(other(role));
        let phase = lock(&self.shared).phase.clone();
        match phase {
            Phase::Idle => self.start(),
            Phase::Starting | Phase::Failed(_) => {}
            Phase::Ready => self.show(role, pane, page, cell_w, cell_h),
        }
    }

    /// The view a pane shows in and the page it loads there.
    fn page_of(&self, pane: &HtmlPane) -> (Role, Page) {
        match &pane.source {
            HtmlSource::Document(document) => (Role::Plain, Page::Document(document.clone())),
            HtmlSource::File { path, root, scripts } => {
                let scripts_failed = matches!(lock(&self.shared).slots[Role::Scripts as usize].phase, Phase::Failed(_));
                let role = if *scripts && !scripts_failed { Role::Scripts } else { Role::Plain };
                match webpane::url_for(root, path) {
                    Some(url) => (role, Page::File { url: to_view(&url), serving: Serving { root: root.clone(), main: path.clone(), scripts: role == Role::Scripts } }),
                    None => {
                        let message = format!("{path} cannot be shown here: it is not a file inside {root}.");
                        (Role::Plain, Page::Document(Arc::new(notes::message_page(&message))))
                    }
                }
            }
        }
    }

    /// Places the view over the pane, loads its page when it changed and shows it.
    fn show(&mut self, role: Role, pane: &HtmlPane, page: Page, cell_w: f32, cell_h: f32) {
        let r = pane.rect;
        let rect = [r.x as f32 * cell_w, r.y as f32 * cell_h, r.width as f32 * cell_w, r.height as f32 * cell_h];
        // The page's CSS pixels follow the window's scale (cells are 18.2 px at 100 %).
        let zoom = ((cell_h / 18.2) * 4.0).round().max(4.0) / 4.0;
        if self.rect.map(|o| (o[2], o[3])) != Some((rect[2], rect[3])) || self.zoom != zoom {
            self.send(json!({ "cmd": "size", "w": rect[2].round(), "h": rect[3].round(), "zoom": zoom }));
        }
        (self.rect, self.zoom) = (Some(rect), zoom);
        let serving = match &page {
            Page::Document(_) => None,
            Page::File { serving, .. } => Some(serving.clone()),
        };
        let shown: Shown = (pane.id.clone(), serving.clone());
        let reload = lock(&self.shared).slots[role as usize].shown.as_ref() != Some(&shown);
        if reload {
            cancel(&self.shared, &self.pipe, role);
            self.next_load += 1;
            let load = self.next_load;
            {
                let mut s = lock(&self.shared);
                let slot = s.slot(role);
                // The view serves the new note (or nothing) before its load starts.
                (slot.serving, slot.shown, slot.loading) = (serving, Some(shown), Some(load));
                if role == Role::Scripts && slot.phase == Phase::Idle {
                    slot.phase = Phase::Ready;
                }
            }
            match page {
                Page::Document(document) => self.send(json!({ "cmd": "load", "view": role.name(), "html": document.as_str(), "load": load })),
                Page::File { url, .. } => self.send(json!({ "cmd": "load_uri", "view": role.name(), "uri": url, "load": load })),
            }
        }
        let shown_now = {
            let mut s = lock(&self.shared);
            let before = s.active == Some(role) && s.slots[role as usize].visible;
            s.active = Some(role);
            s.slot(role).visible = true;
            before
        };
        if !shown_now {
            self.send(json!({ "cmd": "show", "view": role.name() }));
        }
    }

    /// Hides a view. The scripts view also unloads its page: no note's
    /// script runs out of sight. The keyboard goes back to the canvas.
    fn hide(&mut self, role: Role) {
        {
            let mut s = lock(&self.shared);
            let slot = s.slot(role);
            if !slot.visible {
                return;
            }
            slot.visible = false;
            if role == Role::Scripts {
                (slot.shown, slot.serving, slot.loading) = (None, None, None);
            }
        }
        if role == Role::Scripts {
            cancel(&self.shared, &self.pipe, role);
            self.send(json!({ "cmd": "unload", "view": "scripts" }));
        }
        self.blur();
    }

    /// Gives the keyboard back to the canvas if the view has it.
    pub fn blur(&mut self) {
        self.focus(false);
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
            // Printing writes PDF files only: no CUPS or LPR backend is loaded.
            .env("GTK_PRINT_BACKENDS", "file")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => return fail(&self.shared, format!("the HTML view could not start: {error}")),
        };
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else { return fail(&self.shared, "the HTML view has no pipes".into()) };
        *self.pipe.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(stdin);
        lock(&self.shared).jobs = start_readers(self.shared.clone(), self.pipe.clone());
        let (shared, pipe) = (self.shared.clone(), self.pipe.clone());
        let _ = std::thread::Builder::new().name("html-view".into()).spawn(move || read(BufReader::new(stdout), &shared, &pipe));
        self.child = Some(child);
    }

    /// Prints a page that waited for its load.
    pub fn pump(&mut self) {
        if lock(&self.shared).print {
            self.run_print();
        }
    }

    /// *Print / PDF*: the shown page as `<Downloads>/<title>.pdf`, once it has loaded.
    pub fn print(&mut self) {
        lock(&self.shared).print = true;
        self.run_print();
    }

    fn run_print(&mut self) {
        let title = {
            let mut s = lock(&self.shared);
            if let Phase::Failed(why) = &s.phase {
                let why = format!("Printing needs the HTML view: {why}.");
                s.print = false;
                s.events.push(WebEvent::Notice(why, true));
                drop(s);
                return wake();
            }
            let Some(role) = s.active.filter(|r| s.slots[*r as usize].visible && s.slots[*r as usize].loading.is_none()) else { return };
            if s.phase != Phase::Ready || s.printing.is_some() {
                return;
            }
            s.print = false;
            s.slots[role as usize].title.clone()
        };
        let dir = downloads(&self.home);
        match webpane::pdf_path(&dir, &title, webpane::claim_new) {
            Ok(path) => {
                lock(&self.shared).printing = Some(path.clone());
                self.send(json!({ "cmd": "print", "path": path }));
            }
            Err(why) => {
                lock(&self.shared).events.push(WebEvent::Notice(format!("The PDF could not be saved: {why}"), true));
                wake();
            }
        }
    }

    /// The view's state for the test hooks.
    pub fn json(&self) -> Value {
        let s = lock(&self.shared);
        let role = s.active.unwrap_or(Role::Plain);
        let slot = &s.slots[role as usize];
        let views: Vec<Value> = [Role::Plain, Role::Scripts]
            .iter()
            .map(|r| {
                let v = &s.slots[*r as usize];
                let serving = v.serving.as_ref().map(|x| json!({ "root": x.root, "main": x.main, "scripts": x.scripts }));
                let phase = if *r == Role::Plain { &s.phase } else { &v.phase };
                json!({ "view": r.name(), "phase": phase_text(phase), "visible": v.visible, "shown": v.shown.as_ref().map(|(id, _)| id), "serving": serving, "loading": v.loading.is_some(), "focused": v.links.focused() })
            })
            .collect();
        json!({
            "ok": true,
            "phase": phase_text(&s.phase),
            "engine": format!("WebKitGTK {} (offscreen{})", s.webkit, if s.sandbox { ", sandboxed" } else { "" }),
            "visible": slot.visible,
            "bounds": self.rect.map(|r| [r[0].round() as i32, r[1].round() as i32, (r[0] + r[2]).round() as i32, (r[1] + r[3]).round() as i32]),
            "report": slot.shown.as_ref().map(|(id, _)| id),
            "view": role.name(),
            "views": views,
            "title": slot.title,
            "source": slot.source,
            "loads": s.loads,
            "frames": s.frames,
            "focused": self.focused,
            "blocked_navigations": s.blocked_navigations,
            "blocked_requests": s.blocked_requests,
            "blocked_request_uris": s.blocked_request_uris,
            "served_requests": s.served,
            "served_request_uris": s.served_request_uris,
            "pending_requests": s.pending.len(),
            "app_links": s.app_links,
            "print_pending": s.print,
            "printed": s.printed,
            "blocked_windows": s.blocked_windows,
            "blocked_downloads": s.blocked_downloads,
            "denied_permissions": s.denied_permissions,
            "copied": s.copied,
            "folder": self.home.join("webkit"),
        })
    }

    /// Runs script in the shown page as the host (test hooks only: the plain
    /// view's pages never run their own) and answers its result through `reply`.
    pub fn eval(&mut self, script: &str, reply: Sender<Value>) {
        if self.child.is_none() || lock(&self.shared).phase != Phase::Ready {
            let _ = reply.send(json!({ "ok": false, "error": "no HTML view" }));
            return;
        }
        self.next_eval += 1;
        let id = self.next_eval;
        lock(&self.shared).replies.insert(id, reply);
        self.send(json!({ "cmd": "eval", "id": id, "script": script }));
    }

    /// Loads HTML as it is in the plain view, without the canvas's sanitizer
    /// (test hooks only: checks that the view's own lock-down holds by itself).
    pub fn probe_raw(&mut self, html: &str) -> Value {
        if self.child.is_none() {
            return json!({ "ok": false, "error": "no HTML view" });
        }
        cancel(&self.shared, &self.pipe, Role::Plain);
        lock(&self.shared).slot(Role::Plain).serving = None;
        self.send(json!({ "cmd": "load", "view": "plain", "html": html }));
        json!({ "ok": true })
    }

    /// What the view shows as a PNG file (its latest frame).
    pub fn capture(&self, path: PathBuf, reply: Sender<Value>) {
        let frame = {
            let s = lock(&self.shared);
            s.active.and_then(|r| s.slots[r as usize].frame.clone())
        };
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
        // Reads not yet started are skipped; the helper goes with its view.
        for slot in &lock(&self.shared).slots {
            slot.generation.fetch_add(1, Ordering::Relaxed);
        }
        lock(&self.shared).jobs = None;
        *self.pipe.0.lock().unwrap_or_else(|p| p.into_inner()) = None;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The composed window is the GUI's own frame here (the view is part of it):
/// the shell renders it offscreen instead.
pub fn capture_window(_window: isize, _path: &Path) -> anyhow::Result<(u32, u32)> {
    anyhow::bail!("the HTML view is drawn by the GUI: capture its own frame")
}

fn other(role: Role) -> Role {
    match role {
        Role::Plain => Role::Scripts,
        Role::Scripts => Role::Plain,
    }
}

/// What a view loads for a pane.
enum Page {
    /// A whole page as a string.
    Document(Arc<String>),
    /// An HTML note at its URL of the notes scheme.
    File { url: String, serving: Serving },
}

/// Where *Print / PDF* saves: the user's Downloads folder (XDG's
/// `XDG_DOWNLOAD_DIR`, else `~/Downloads`, else the temporary folder);
/// under the test hooks `<home>/downloads`, never the user's.
fn downloads(home: &Path) -> PathBuf {
    if std::env::args().any(|a| a == "--test-hooks") {
        let dir = home.join("downloads");
        let _ = std::fs::create_dir_all(&dir);
        return dir;
    }
    let user_home = std::env::var_os("HOME").map(PathBuf::from);
    let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| user_home.as_ref().map(|h| h.join(".config")));
    let xdg = config.and_then(|c| std::fs::read_to_string(c.join("user-dirs.dirs")).ok()).and_then(|text| xdg_download_dir(&text, user_home.as_deref()?));
    xdg.into_iter().chain(user_home.map(|h| h.join("Downloads"))).find(|d| d.is_dir()).unwrap_or_else(std::env::temp_dir)
}

/// `XDG_DOWNLOAD_DIR` from `user-dirs.dirs` (`$HOME/…` or an absolute path).
fn xdg_download_dir(text: &str, home: &Path) -> Option<PathBuf> {
    let line = text.lines().map(str::trim).find(|l| l.starts_with("XDG_DOWNLOAD_DIR="))?;
    let value = line["XDG_DOWNLOAD_DIR=".len()..].trim_matches('"');
    if let Some(rest) = value.strip_prefix("$HOME") {
        let rest = rest.trim_start_matches('/');
        return Some(if rest.is_empty() { home.to_path_buf() } else { home.join(rest) });
    }
    value.starts_with('/').then(|| PathBuf::from(value))
}

/// A view's page is replaced: the requests it still waits on are answered
/// (403) now, and its reads not yet started are skipped.
fn cancel(shared: &Mutex<Shared>, pipe: &Pipe, role: Role) {
    let stale: Vec<u64> = {
        let mut s = lock(shared);
        s.slot(role).generation.fetch_add(1, Ordering::Relaxed);
        let ids: Vec<u64> = s.pending.iter().filter(|(_, r)| **r == role).map(|(id, _)| *id).collect();
        for id in &ids {
            s.pending.remove(id);
        }
        ids
    };
    for id in stale {
        pipe.reply(id, &webpane::refused());
    }
}

/// Starts the threads that read notes' files and answer their requests
/// (`None`: none could start). A job of a replaced page is skipped (its
/// request was answered already).
fn start_readers(shared: Arc<Mutex<Shared>>, pipe: Arc<Pipe>) -> Option<crossbeam_channel::Sender<Job>> {
    let (jobs, queue) = crossbeam_channel::unbounded::<Job>();
    let mut started = 0;
    for n in 0..READERS {
        let (queue, shared, pipe) = (queue.clone(), shared.clone(), pipe.clone());
        let spawned = std::thread::Builder::new().name(format!("note-files-{n}")).spawn(move || {
            while let Ok(job) = queue.recv() {
                if job.live.load(Ordering::Relaxed) != job.generation {
                    continue;
                }
                let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let read = job.backend.read_bytes(&job.path, canvas_model::notebook::MAX_BYTES).map_err(|e| format!("{e:#}"));
                    webpane::reply(&job.path, job.main, job.scripts, read)
                }));
                let reply = made.unwrap_or_else(|_| webpane::reply(&job.path, job.main, job.scripts, Err("reading it failed".into())));
                // Still waiting (not answered when its page was replaced)?
                if lock(&shared).pending.remove(&job.id).is_some() {
                    pipe.reply(job.id, &for_view(reply, job.main, job.scripts));
                }
            }
        });
        started += usize::from(spawned.is_ok());
    }
    (started > 0).then_some(jobs)
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
    wake();
}

/// The helper's records: events and frames.
fn read(mut out: impl Read, shared: &Mutex<Shared>, pipe: &Pipe) {
    while let Some((kind, mut body)) = read_record(&mut out) {
        match kind {
            b'F' if body.len() >= 9 => {
                let role = if body[0] == Role::Scripts as u8 { Role::Scripts } else { Role::Plain };
                let (w, h) = (u32::from_le_bytes(body[1..5].try_into().unwrap_or_default()), u32::from_le_bytes(body[5..9].try_into().unwrap_or_default()));
                let rgba = body.split_off(9);
                if rgba.len() == (w * h * 4) as usize {
                    let mut s = lock(shared);
                    s.frames += 1;
                    let key = FRAME_KEYS | s.frames;
                    s.slot(role).frame = Some((key, w, h, Arc::new(rgba)));
                }
            }
            b'J' => {
                let Ok(value) = serde_json::from_slice::<Value>(&body) else { continue };
                apply(shared, pipe, &value);
            }
            _ => {}
        }
        wake();
    }
    fail(shared, "the HTML view's helper ended (WebKitGTK missing or failed; see the GUI's log)".into());
}

/// A request of the notes scheme: a file of the note's folder is read
/// through the backend (answered by a reader), everything else 403 now.
fn request(shared: &Mutex<Shared>, pipe: &Pipe, id: u64, role: Role, uri: &str, method: &str) {
    let origin = to_origin(uri);
    let mut s = lock(shared);
    let serving = s.slots[role as usize].serving.clone();
    // Frames are refused before they load (only the page's own load is let through), so a request here is a resource.
    let answer = match webpane::classify(&origin, method, false, serving.as_ref()) {
        Resource::File { path, main } if webpane::admit(s.pending.len()) => {
            let scripts = role == Role::Scripts;
            let unread = |path: &str| for_view(webpane::reply(path, main, scripts, Err("the files cannot be read now".into())), main, scripts);
            match (s.backend.clone(), s.jobs.clone()) {
                (Some(backend), Some(jobs)) => {
                    let live = s.slots[role as usize].generation.clone();
                    let generation = live.load(Ordering::Relaxed);
                    s.pending.insert(id, role);
                    if jobs.send(Job { id, path: path.clone(), main, scripts, backend, live, generation }).is_ok() {
                        s.served += 1;
                        push_capped(&mut s.served_request_uris, origin);
                        return;
                    }
                    s.pending.remove(&id);
                    unread(&path)
                }
                _ => unread(&path),
            }
        }
        Resource::File { .. } => {
            s.blocked_requests += 1;
            webpane::busy()
        }
        Resource::Refused => {
            s.blocked_requests += 1;
            push_capped(&mut s.blocked_request_uris, origin);
            webpane::refused()
        }
    };
    drop(s);
    pipe.reply(id, &answer);
}

/// A link the user followed in `role`'s view (refused there): the canvas acts on it.
fn asked(s: &mut Shared, role: Role, uri: &str) {
    let root = s.slots[role as usize].serving.as_ref().map(|serving| serving.root.clone());
    let event = match webpane::link_action(&to_origin(uri), root.as_deref()) {
        LinkAction::App(space, target) => {
            s.app_links += 1;
            WebEvent::Navigate(space, target)
        }
        LinkAction::Browser(url) => WebEvent::Link(url),
        LinkAction::Ignore => return push_capped(&mut s.blocked_navigations, uri.to_owned()),
    };
    s.events.push(event);
}

fn apply(shared: &Mutex<Shared>, pipe: &Pipe, value: &Value) {
    let text = |k: &str| value[k].as_str().unwrap_or_default().to_owned();
    let role = Role::of(value["view"].as_str().unwrap_or("plain"));
    match value["event"].as_str().unwrap_or_default() {
        "ready" => {
            let mut s = lock(shared);
            s.phase = Phase::Ready;
            s.slot(Role::Plain).phase = Phase::Ready;
            s.webkit = text("webkit");
            s.sandbox = value["sandbox"].as_bool().unwrap_or(false);
            s.events.push(WebEvent::Ready);
        }
        "failed" => fail(shared, text("why")),
        "view_failed" => {
            // The scripts view cannot work: notes allowed to run scripts show in the plain view, without them.
            cancel(shared, pipe, role);
            let mut s = lock(shared);
            let slot = s.slot(role);
            (slot.phase, slot.shown, slot.serving, slot.loading, slot.visible) = (Phase::Failed(text("why")), None, None, None, false);
            let why = format!("Scripts in HTML notes are unavailable ({}); such notes show without them.", text("why"));
            s.events.push(WebEvent::Notice(why, true));
        }
        "loaded" => {
            let mut s = lock(shared);
            s.loads = value["loads"].as_u64().unwrap_or(0) as u32;
            let load = value["load"].as_u64().unwrap_or(0);
            let slot = s.slot(role);
            (slot.title, slot.source) = (text("title"), text("source"));
            if slot.loading == Some(load) {
                slot.loading = None;
            }
        }
        "request" => request(shared, pipe, value["id"].as_u64().unwrap_or(0), role, &text("uri"), &text("method")),
        "link" => asked(&mut lock(shared), role, &text("url")),
        "fragment" => {
            // The page's own URL would name a place in the app: followed in the
            // plain view while shown, in the scripts view once per click into it.
            let url = text("url");
            let mut s = lock(shared);
            let visible = s.slots[role as usize].visible;
            let target = webpane::fragment_target(&to_origin(&url));
            if let Some((space, target)) = target.filter(|_| s.slot(role).links.follow(role == Role::Scripts, visible, Instant::now())) {
                s.app_links += 1;
                s.events.push(WebEvent::Navigate(space, target));
            } else {
                push_capped(&mut s.blocked_navigations, url);
            }
        }
        "blocked_navigation" => push_capped(&mut lock(shared).blocked_navigations, text("url")),
        "blocked_request" => {
            let mut s = lock(shared);
            s.blocked_requests += 1;
            push_capped(&mut s.blocked_request_uris, text("request"));
        }
        "copied" => {
            let text = text("text");
            if !text.is_empty() {
                let _ = canvas_sys::clipboard::set_text(&text);
            }
            lock(shared).copied = text;
        }
        "printed" => {
            let mut s = lock(shared);
            let path = s.printing.take().unwrap_or_else(|| PathBuf::from(text("path")));
            let written = std::fs::metadata(&path).is_ok_and(|m| m.len() > 0);
            let event = if value["ok"] == true && written {
                s.printed = Some(path.clone());
                WebEvent::Notice(format!("Saved as PDF: {}", path.display()), false)
            } else {
                // Our claim, still empty: no file is left behind.
                if std::fs::metadata(&path).is_ok_and(|m| m.len() == 0) {
                    let _ = std::fs::remove_file(&path);
                }
                let why = text("error");
                WebEvent::Notice(format!("The PDF could not be saved: {}", if why.is_empty() { "nothing was written".into() } else { why }), true)
            };
            s.events.push(event);
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

/// What a key pressed in the page does for the canvas (`webpane::web_key`:
/// Esc, F6, Ctrl+], Ctrl+1/2/3, Alt+←/→, …), from winit's key as Windows'
/// virtual key and the character it gives unmodified.
fn web_key_of(key: &winit::keyboard::Key, mods: winit::keyboard::ModifiersState) -> Option<WebKey> {
    use winit::keyboard::{Key, NamedKey};
    let (vk, ch) = match key {
        Key::Named(NamedKey::Escape) => (webpane::VK_ESCAPE, None),
        Key::Named(NamedKey::F6) => (webpane::VK_F6, None),
        Key::Named(NamedKey::Tab) => (webpane::VK_TAB, None),
        Key::Named(NamedKey::ArrowLeft) => (webpane::VK_LEFT, None),
        Key::Named(NamedKey::ArrowRight) => (webpane::VK_RIGHT, None),
        Key::Character(text) => {
            let mut chars = text.chars();
            let c = chars.next()?.to_ascii_lowercase();
            if chars.next().is_some() {
                return None;
            }
            match c {
                'a'..='z' => (c.to_ascii_uppercase() as u32, Some(c)),
                '0'..='9' => (c as u32, Some(c)),
                // Other characters: no virtual key of their own here (`,` `.` `]`).
                c => (0, Some(c)),
            }
        }
        _ => return None,
    };
    webpane::web_key(vk, ch, mods.control_key(), mods.shift_key(), mods.alt_key())
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

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::{Key, ModifiersState, NamedKey};

    #[test]
    fn the_notes_origin_and_the_views_scheme_map_both_ways() {
        let url = "https://notes.canvas.invalid/human-outputs/page%20one.html#top";
        let view = to_view(url);
        assert_eq!(view, "canvas-notes://notes.canvas.invalid/human-outputs/page%20one.html#top");
        assert_eq!(to_origin(&view), url);
        assert_eq!(to_origin("CANVAS-NOTES://NOTES.CANVAS.INVALID/x"), "https://notes.canvas.invalid/x");
        for other in ["https://example.com/", "about:blank#note:x", "data:text/html,x", "canvas-notes://evil.example/x"] {
            assert_eq!(to_origin(other), other);
            assert_eq!(to_view(other), other);
        }
        // Only the note's folder is served, as webpane decides on the origin's form.
        let serving = Serving { root: "/n".into(), main: "/n/a/page.html".into(), scripts: false };
        assert_eq!(webpane::classify(&to_origin("canvas-notes://notes.canvas.invalid/a/style.css"), "GET", false, Some(&serving)), Resource::File { path: "/n/a/style.css".into(), main: false });
        assert_eq!(webpane::classify(&to_origin("canvas-notes://notes.canvas.invalid/b/x.css"), "GET", false, Some(&serving)), Resource::Refused);
        assert_eq!(webpane::classify(&to_origin("canvas-notes://notes.canvas.invalid/a/page.html"), "POST", false, Some(&serving)), Resource::Refused);
    }

    #[test]
    fn replies_name_the_views_origin() {
        let page = webpane::reply("/n/a/page.html", true, false, Ok(b"<p><img src=\"https://notes.canvas.invalid/a/i.png\"></p>".to_vec()));
        let served = for_view(page, true, false);
        let body = String::from_utf8(served.body).unwrap();
        assert!(served.headers.contains("img-src canvas-notes://notes.canvas.invalid") && !served.headers.contains(notes::ORIGIN), "{}", served.headers);
        assert!(body.contains("canvas-notes://notes.canvas.invalid/a/i.png") && !body.contains(notes::ORIGIN), "{body}");
        // A note whose scripts may run is served as it is (its policy still the view's).
        let raw = for_view(webpane::reply("/n/a/page.html", true, true, Ok(b"https://notes.canvas.invalid/x".to_vec())), true, true);
        assert_eq!(raw.body, b"https://notes.canvas.invalid/x");
        assert!(raw.headers.contains("sandbox allow-scripts") && raw.headers.contains("script-src 'unsafe-inline' canvas-notes://notes.canvas.invalid"));
    }

    #[test]
    fn global_keys_go_to_the_canvas() {
        let ctrl = ModifiersState::CONTROL;
        let alt = ModifiersState::ALT;
        let none = ModifiersState::empty();
        assert!(matches!(web_key_of(&Key::Named(NamedKey::Escape), none), Some(WebKey::Escape)));
        for (key, mods) in [
            (Key::Named(NamedKey::F6), none),
            (Key::Character("]".into()), ctrl),
            (Key::Character("2".into()), ctrl),
            (Key::Character("p".into()), ctrl),
            (Key::Character("N".into()), ctrl | ModifiersState::SHIFT),
            (Key::Named(NamedKey::ArrowLeft), alt),
            (Key::Character(",".into()), alt),
            (Key::Character(".".into()), alt),
            (Key::Character("n".into()), alt),
            (Key::Named(NamedKey::Tab), ctrl),
        ] {
            assert!(matches!(web_key_of(&key, mods), Some(WebKey::Forward(_))), "{key:?} {mods:?}");
        }
        // The page keeps its own keys: copy, select all, find in page, arrows, typing, AltGr.
        for (key, mods) in [(Key::Character("c".into()), ctrl), (Key::Character("a".into()), ctrl), (Key::Named(NamedKey::ArrowLeft), none), (Key::Character("x".into()), none), (Key::Character("4".into()), ctrl), (Key::Character("]".into()), ctrl | alt)] {
            assert!(web_key_of(&key, mods).is_none(), "{key:?} {mods:?}");
        }
    }

    #[test]
    fn downloads_follow_xdg_user_dirs() {
        let home = Path::new("/home/u");
        assert_eq!(xdg_download_dir("# comment\nXDG_DOWNLOAD_DIR=\"$HOME/Téléchargements\"\n", home), Some(PathBuf::from("/home/u/Téléchargements")));
        assert_eq!(xdg_download_dir("XDG_DOWNLOAD_DIR=\"/data/dl\"", home), Some(PathBuf::from("/data/dl")));
        assert_eq!(xdg_download_dir("XDG_DOWNLOAD_DIR=\"$HOME/\"", home), Some(PathBuf::from("/home/u")));
        assert_eq!(xdg_download_dir("XDG_DESKTOP_DIR=\"$HOME/Desktop\"", home), None);
        assert_eq!(xdg_download_dir("XDG_DOWNLOAD_DIR=\"relative\"", home), None);
    }
}
