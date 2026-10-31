//! The HTML view's helper process (MAC-LINUX-PLAN §5.7, D9; the spike's
//! decision in PORT-STATUS): `copilot-canvas-gui` started again with
//! `COPILOT_CANVAS_HTML_VIEW=1` runs WebKitGTK 4.1 in a `GtkOffscreenWindow`
//! (no surface on screen; its own display connection, X11 or Wayland alike)
//! and talks to the GUI over its standard streams:
//!
//! - in: JSON lines (`load`, `size`, `wheel`, `motion`, `button`, `key`,
//!   `eval`);
//! - out: records of a kind byte and a little-endian length: `J` a JSON event
//!   (`ready`, `failed`, `loaded`, `link`, `blocked_navigation`,
//!   `blocked_request`, `blocked_window`, `reply`), `F` a frame (width,
//!   height, straight RGBA rows).
//!
//! A crash of GTK or WebKit (WebKit aborts when its bubblewrap sandbox cannot
//! start) ends this process only; the GUI then shows reports as formatted
//! text. The view is locked down in layers on top of the core's sanitizer and
//! the document's Content-Security-Policy:
//!
//! - page JavaScript off (`enable-javascript`, and `enable-javascript-markup`
//!   removes script elements and handler attributes while parsing); the test
//!   hooks' `eval` turns the API's JavaScript on only while it runs;
//! - every http(s) and ws(s) request goes to a local proxy of this process
//!   that answers 403 (also WebKit's preconnects to a clicked link's host);
//!   `file:`, `blob:` and `ftp:` loads are blocked by a content rule list;
//! - every navigation but the GUI's own load of the page is refused; a link
//!   the user clicks becomes a request to the canvas (which asks first); no
//!   new windows, downloads cancelled, permissions denied, no context menu, no
//!   developer extras; an ephemeral context (nothing stored);
//! - WebKit's bubblewrap sandbox when a probe shows bubblewrap can run here.

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use std::ptr::null;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::gtk::*;

/// The environment variable that makes the GUI binary this helper.
pub const ENV: &str = "COPILOT_CANVAS_HTML_VIEW";
/// The data home (the content rule list's compiled store lives under it).
pub const HOME_ENV: &str = "COPILOT_CANVAS_HTML_VIEW_HOME";

static GTK: std::sync::OnceLock<Gtk> = std::sync::OnceLock::new();

fn gtk() -> &'static Gtk {
    GTK.get().expect("loaded before use")
}

fn cstr(text: &str) -> CString {
    CString::new(text.replace('\0', "")).unwrap_or_default()
}

fn text(ptr: *const c_char) -> String {
    if ptr.is_null() { String::new() } else { unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned() }
}

/// What the main loop and the signal handlers share (the GTK thread only).
struct State {
    view: gpointer,
    window: gpointer,
    settings: gpointer,
    out: Sender<Out>,
    /// The GUI's own next navigation (its page) may start.
    expect: bool,
    damaged: bool,
    loads: u32,
    width: i32,
    height: i32,
    surface: gpointer,
    /// Pending `eval` replies, by id.
    evals: Vec<u64>,
}

/// A record for the GUI.
enum Out {
    Event(Value),
    Frame(u32, u32, Vec<u8>),
}

/// Sends an event; the state is borrowed only for the moment it takes (GTK
/// and WebKit calls may run other handlers synchronously).
fn event(state: &RefCell<State>, value: Value) {
    let out = state.borrow().out.clone();
    let _ = out.send(Out::Event(value));
}

/// The helper's entry: answers the process's exit code.
pub fn main() -> i32 {
    let (out_tx, out_rx) = channel::<Out>();
    // One writer thread owns stdout.
    let writer = std::thread::spawn(move || {
        let mut stdout = std::io::stdout().lock();
        for record in out_rx {
            let (kind, body) = match record {
                Out::Event(value) => (b'J', value.to_string().into_bytes()),
                Out::Frame(w, h, rgba) => {
                    let mut body = Vec::with_capacity(8 + rgba.len());
                    body.extend(w.to_le_bytes());
                    body.extend(h.to_le_bytes());
                    body.extend(rgba);
                    (b'F', body)
                }
            };
            if stdout.write_all(&[kind]).and_then(|_| stdout.write_all(&(body.len() as u32).to_le_bytes())).and_then(|_| stdout.write_all(&body)).and_then(|_| stdout.flush()).is_err() {
                break;
            }
        }
    });
    let code = match run(out_tx.clone()) {
        Ok(()) => 0,
        Err(why) => {
            let _ = out_tx.send(Out::Event(json!({ "event": "failed", "why": why })));
            1
        }
    };
    drop(out_tx);
    let _ = writer.join();
    code
}

/// Whether bubblewrap can make the namespaces WebKit's sandbox needs here
/// (Ubuntu 24.04's AppArmor rules refuse unprivileged user namespaces, and
/// WebKit then aborts instead of starting).
fn sandbox_works() -> bool {
    std::process::Command::new("bwrap")
        .args(["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--unshare-all", "--die-with-parent", "true"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The local proxy every http(s)/ws(s) request goes to: 403 for all.
fn deny_proxy(out: Sender<Out>) -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut head = Vec::new();
            let mut buf = [0u8; 1024];
            while head.len() < 16 * 1024 && !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            let first = String::from_utf8_lossy(&head).lines().next().unwrap_or_default().chars().take(300).collect::<String>();
            let _ = stream.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = out.send(Out::Event(json!({ "event": "blocked_request", "request": first })));
            if let Some(gtk) = GTK.get() {
                // SAFETY: waking the default main context is thread safe.
                unsafe { (gtk.g_main_context_wakeup)(std::ptr::null_mut()) };
            }
        }
    });
    Ok(port)
}

const RULES: &str = r#"[{"trigger":{"url-filter":"^file:"},"action":{"type":"block"}},{"trigger":{"url-filter":"^blob:"},"action":{"type":"block"}},{"trigger":{"url-filter":"^ftp:"},"action":{"type":"block"}}]"#;

struct FilterSaved {
    done: bool,
    filter: gpointer,
    error: String,
}

unsafe extern "C" fn on_filter_saved(store: gpointer, result: gpointer, data: gpointer) {
    let saved = unsafe { &mut *(data as *mut FilterSaved) };
    let mut error: *mut GError = std::ptr::null_mut();
    saved.filter = unsafe { (gtk().webkit_user_content_filter_store_save_finish)(store, result, &mut error) };
    if !error.is_null() {
        saved.error = text(unsafe { (*error).message });
        unsafe { (gtk().g_error_free)(error) };
    }
    saved.done = true;
}

fn set_bool(object: gpointer, name: &str, value: bool) {
    let name = cstr(name);
    // SAFETY: a boolean GObject property; one property per call, so an
    // unknown name (an older WebKit) only warns and skips itself.
    unsafe { (gtk().g_object_set)(object, name.as_ptr(), value as gboolean, null::<c_char>()) };
}

fn set_int(object: gpointer, name: &str, value: c_int) {
    let name = cstr(name);
    // SAFETY: as above, an enum (int) property.
    unsafe { (gtk().g_object_set)(object, name.as_ptr(), value, null::<c_char>()) };
}

fn connect(object: gpointer, signal: &str, handler: *const c_void, data: gpointer) {
    let signal = cstr(signal);
    // SAFETY: the handler's signature matches the signal's (each below).
    unsafe { (gtk().g_signal_connect_data)(object, signal.as_ptr(), handler, data, std::ptr::null_mut(), 0) };
}

fn state_of(data: gpointer) -> &'static RefCell<State> {
    // SAFETY: `data` is the leaked state handed to every handler.
    unsafe { &*(data as *const RefCell<State>) }
}

unsafe extern "C" fn on_load_changed(view: gpointer, load_event: c_int, data: gpointer) {
    if load_event == WEBKIT_LOAD_FINISHED {
        let state = state_of(data);
        let loads = {
            let mut s = state.borrow_mut();
            s.loads += 1;
            s.damaged = true;
            s.loads
        };
        let (title, uri) = unsafe { (text((gtk().webkit_web_view_get_title)(view)), text((gtk().webkit_web_view_get_uri)(view))) };
        event(state, json!({ "event": "loaded", "loads": loads, "title": title, "source": uri }));
    }
}

unsafe extern "C" fn on_decide_policy(_view: gpointer, decision: gpointer, kind: c_int, data: gpointer) -> gboolean {
    if kind == WEBKIT_POLICY_DECISION_TYPE_RESPONSE {
        return 0;
    }
    let g = gtk();
    let state = state_of(data);
    // SAFETY: a navigation or new-window decision of this view.
    unsafe {
        let action = (g.webkit_navigation_policy_decision_get_navigation_action)(decision);
        let uri = text((g.webkit_uri_request_get_uri)((g.webkit_navigation_action_get_request)(action)));
        let own = uri.starts_with("about:blank") || uri.starts_with("data:");
        let expected = state.borrow().expect;
        if kind == 0 && own && (expected || uri.starts_with("about:blank#")) {
            state.borrow_mut().expect = false;
            (g.webkit_policy_decision_use)(decision);
            return 1;
        }
        let clicked = (g.webkit_navigation_action_get_navigation_type)(action) == WEBKIT_NAVIGATION_TYPE_LINK_CLICKED && (g.webkit_navigation_action_is_user_gesture)(action) != 0;
        if clicked && !own {
            event(state, json!({ "event": "link", "url": uri }));
        } else {
            event(state, json!({ "event": "blocked_navigation", "url": uri }));
        }
        (g.webkit_policy_decision_ignore)(decision);
    }
    1
}

unsafe extern "C" fn on_create(_view: gpointer, _action: gpointer, data: gpointer) -> gpointer {
    event(state_of(data), json!({ "event": "blocked_window" }));
    std::ptr::null_mut()
}

unsafe extern "C" fn on_permission(_view: gpointer, request: gpointer, data: gpointer) -> gboolean {
    unsafe { (gtk().webkit_permission_request_deny)(request) };
    event(state_of(data), json!({ "event": "denied_permission" }));
    1
}

unsafe extern "C" fn on_context_menu(_view: gpointer, _menu: gpointer, _event: gpointer, _hit: gpointer, _data: gpointer) -> gboolean {
    1
}

unsafe extern "C" fn on_download(_context: gpointer, download: gpointer, data: gpointer) {
    unsafe { (gtk().webkit_download_cancel)(download) };
    event(state_of(data), json!({ "event": "blocked_download" }));
}

unsafe extern "C" fn on_terminated(_view: gpointer, reason: c_int, data: gpointer) {
    event(state_of(data), json!({ "event": "failed", "why": format!("WebKit's web process ended ({reason})") }));
}

unsafe extern "C" fn on_damage(_widget: gpointer, _event: gpointer, data: gpointer) -> gboolean {
    state_of(data).borrow_mut().damaged = true;
    0
}

struct EvalCall {
    state: gpointer,
    id: u64,
    /// A copy of the selection rather than a hook's evaluation.
    copy: bool,
}

unsafe extern "C" fn on_evaluated(view: gpointer, result: gpointer, data: gpointer) {
    let call = unsafe { Box::from_raw(data as *mut EvalCall) };
    let g = gtk();
    let mut error: *mut GError = std::ptr::null_mut();
    let value = unsafe { (g.webkit_web_view_evaluate_javascript_finish)(view, result, &mut error) };
    let reply = if call.copy {
        let text = if error.is_null() && !value.is_null() {
            let raw = unsafe { (g.jsc_value_to_string)(value) };
            let text = self::text(raw);
            unsafe {
                (g.g_free)(raw as gpointer);
                (g.g_object_unref)(value);
            }
            text
        } else {
            if !error.is_null() {
                unsafe { (g.g_error_free)(error) };
            }
            String::new()
        };
        json!({ "event": "copied", "text": text })
    } else if !error.is_null() {
        let message = text(unsafe { (*error).message });
        unsafe { (g.g_error_free)(error) };
        json!({ "event": "reply", "id": call.id, "ok": false, "error": message })
    } else if value.is_null() {
        json!({ "event": "reply", "id": call.id, "ok": true, "result": null })
    } else {
        let raw = unsafe { (g.jsc_value_to_string)(value) };
        let result = text(raw);
        unsafe {
            (g.g_free)(raw as gpointer);
            (g.g_object_unref)(value);
        }
        json!({ "event": "reply", "id": call.id, "ok": true, "result": result })
    };
    let state = state_of(call.state);
    let (settings, done) = {
        let mut s = state.borrow_mut();
        s.evals.retain(|id| *id != call.id);
        (s.settings, s.evals.is_empty())
    };
    // The page's own JavaScript stays off: the API's is on only while evaluating.
    if done {
        set_bool(settings, "enable-javascript", false);
    }
    event(state, reply);
}

fn run(out: Sender<Out>) -> Result<(), String> {
    let g = Gtk::load()?;
    let _ = GTK.set(g);
    let g = gtk();
    // SAFETY: GTK starts on this thread, which then runs its main loop.
    if unsafe { (g.gtk_init_check)(std::ptr::null_mut(), std::ptr::null_mut()) } == 0 {
        return Err("GTK cannot open the display".into());
    }
    let version = unsafe { format!("{}.{}.{}", (g.webkit_get_major_version)(), (g.webkit_get_minor_version)(), (g.webkit_get_micro_version)()) };
    let sandbox = sandbox_works();
    let port = deny_proxy(out.clone()).map_err(|e| format!("cannot start the request guard: {e}"))?;
    let home = std::env::var_os(HOME_ENV).map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    // SAFETY: plain WebKitGTK calls on the GTK thread with valid arguments.
    let state = unsafe {
        let context = (g.webkit_web_context_new_ephemeral)();
        if sandbox {
            (g.webkit_web_context_set_sandbox_enabled)(context, 1);
        }
        (g.webkit_web_context_set_cache_model)(context, WEBKIT_CACHE_MODEL_DOCUMENT_VIEWER);
        let proxy = cstr(&format!("http://127.0.0.1:{port}"));
        let settings = (g.webkit_network_proxy_settings_new)(proxy.as_ptr(), null());
        (g.webkit_website_data_manager_set_network_proxy_settings)((g.webkit_web_context_get_website_data_manager)(context), WEBKIT_NETWORK_PROXY_MODE_CUSTOM, settings);
        (g.webkit_network_proxy_settings_free)(settings);

        // The content rule list (compiled once into the home).
        let content = (g.webkit_user_content_manager_new)();
        let store_dir = home.join("webkit").join("content-rules");
        let _ = std::fs::create_dir_all(&store_dir);
        let store = (g.webkit_user_content_filter_store_new)(cstr(&store_dir.display().to_string()).as_ptr());
        let mut saved = FilterSaved { done: false, filter: std::ptr::null_mut(), error: String::new() };
        let bytes = (g.g_bytes_new)(RULES.as_ptr() as *const c_void, RULES.len());
        (g.webkit_user_content_filter_store_save)(store, cstr("canvas-report").as_ptr(), bytes, std::ptr::null_mut(), on_filter_saved as *const c_void, &mut saved as *mut FilterSaved as gpointer);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !saved.done && Instant::now() < deadline {
            (g.g_main_context_iteration)(std::ptr::null_mut(), 0);
            std::thread::sleep(Duration::from_millis(2));
        }
        (g.g_bytes_unref)(bytes);
        if saved.filter.is_null() {
            return Err(format!("the content rule list did not compile: {}", saved.error));
        }
        (g.webkit_user_content_manager_add_filter)(content, saved.filter);

        let settings = (g.webkit_settings_new)();
        for (name, value) in [
            ("enable-javascript", false),
            ("enable-javascript-markup", false),
            ("javascript-can-open-windows-automatically", false),
            ("javascript-can-access-clipboard", false),
            ("enable-developer-extras", false),
            ("enable-page-cache", false),
            ("enable-html5-local-storage", false),
            ("enable-webgl", false),
            ("enable-webaudio", false),
            ("enable-media", false),
            ("enable-fullscreen", false),
            ("enable-back-forward-navigation-gestures", false),
            ("allow-file-access-from-file-urls", false),
            ("allow-universal-access-from-file-urls", false),
        ] {
            set_bool(settings, name, value);
        }
        set_int(settings, "hardware-acceleration-policy", WEBKIT_HARDWARE_ACCELERATION_POLICY_NEVER);

        let view = (g.g_object_new)((g.webkit_web_view_get_type)(), cstr("web-context").as_ptr(), context, cstr("settings").as_ptr(), settings, cstr("user-content-manager").as_ptr(), content, null::<c_char>());
        let window = (g.gtk_offscreen_window_new)();
        (g.gtk_window_set_default_size)(window, 800, 600);
        (g.gtk_widget_set_size_request)(view, 800, 600);
        (g.gtk_container_add)(window, view);
        let state: &'static RefCell<State> = Box::leak(Box::new(RefCell::new(State {
            view,
            window,
            settings,
            out: out.clone(),
            expect: false,
            damaged: false,
            loads: 0,
            width: 800,
            height: 600,
            surface: std::ptr::null_mut(),
            evals: Vec::new(),
        })));
        let data = state as *const RefCell<State> as gpointer;
        connect(view, "load-changed", on_load_changed as *const c_void, data);
        connect(view, "decide-policy", on_decide_policy as *const c_void, data);
        connect(view, "create", on_create as *const c_void, data);
        connect(view, "permission-request", on_permission as *const c_void, data);
        connect(view, "context-menu", on_context_menu as *const c_void, data);
        connect(view, "web-process-terminated", on_terminated as *const c_void, data);
        connect(context, "download-started", on_download as *const c_void, data);
        connect(window, "damage-event", on_damage as *const c_void, data);
        (g.gtk_widget_show_all)(window);
        state
    };
    event(state, json!({ "event": "ready", "webkit": version, "sandbox": sandbox, "proxy_port": port }));

    // Commands from the GUI, read on a thread that wakes the main loop.
    let (tx, rx) = channel::<Option<Value>>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                let _ = tx.send(Some(value));
                // SAFETY: waking the default main context is thread safe.
                unsafe { (gtk().g_main_context_wakeup)(std::ptr::null_mut()) };
            }
        }
        let _ = tx.send(None);
        unsafe { (gtk().g_main_context_wakeup)(std::ptr::null_mut()) };
    });
    main_loop(state, rx);
    Ok(())
}

fn main_loop(state: &'static RefCell<State>, commands: Receiver<Option<Value>>) {
    let g = gtk();
    let mut last_frame = Instant::now() - Duration::from_secs(1);
    let mut next_eval = 0u64;
    loop {
        loop {
            match commands.try_recv() {
                Ok(Some(command)) => command_of(state, &command, &mut next_eval),
                // The GUI went away: so does the helper.
                Ok(None) => return,
                Err(_) => break,
            }
        }
        let damaged = state.borrow().damaged;
        if damaged && last_frame.elapsed() >= Duration::from_millis(16) {
            state.borrow_mut().damaged = false;
            send_frame(state);
            last_frame = Instant::now();
        }
        // SAFETY: the GTK thread's main loop.
        unsafe {
            if state.borrow().damaged {
                // A frame is due soon: do not block.
                (g.g_main_context_iteration)(std::ptr::null_mut(), 0);
                std::thread::sleep(Duration::from_millis(4));
            } else {
                (g.g_main_context_iteration)(std::ptr::null_mut(), 1);
            }
        }
    }
}

/// Draws the view into an image surface and sends it as straight RGBA.
fn send_frame(state: &'static RefCell<State>) {
    let g = gtk();
    let (view, w, h, mut surface, out) = {
        let s = state.borrow();
        (s.view, s.width.max(1), s.height.max(1), s.surface, s.out.clone())
    };
    // SAFETY: cairo and GTK calls on the GTK thread; the surface is ours.
    unsafe {
        if surface.is_null() {
            // CAIRO_FORMAT_ARGB32.
            surface = (g.cairo_image_surface_create)(0, w, h);
            state.borrow_mut().surface = surface;
        }
        let cr = (g.cairo_create)(surface);
        (g.gtk_widget_draw)(view, cr);
        (g.cairo_destroy)(cr);
        (g.cairo_surface_flush)(surface);
        let (sw, sh, stride) = ((g.cairo_image_surface_get_width)(surface), (g.cairo_image_surface_get_height)(surface), (g.cairo_image_surface_get_stride)(surface));
        let data = (g.cairo_image_surface_get_data)(surface);
        if data.is_null() || sw <= 0 || sh <= 0 {
            return;
        }
        let bytes = std::slice::from_raw_parts(data, (stride * sh) as usize);
        let mut rgba = Vec::with_capacity((sw * sh * 4) as usize);
        for row in bytes.chunks(stride as usize).take(sh as usize) {
            for px in row[..(sw * 4) as usize].chunks(4) {
                // Premultiplied BGRA (little-endian ARGB32) → straight RGBA.
                let a = px[3];
                let un = |c: u8| if a == 0 || a == 255 { c } else { ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8 };
                rgba.extend([un(px[2]), un(px[1]), un(px[0]), a]);
            }
        }
        let _ = out.send(Out::Frame(sw as u32, sh as u32, rgba));
    }
}

fn command_of(state: &'static RefCell<State>, command: &Value, next_eval: &mut u64) {
    let g = gtk();
    let view = state.borrow().view;
    match command["cmd"].as_str().unwrap_or_default() {
        "load" => {
            let html = cstr(command["html"].as_str().unwrap_or_default());
            state.borrow_mut().expect = true;
            // SAFETY: the page is copied by WebKit.
            unsafe { (g.webkit_web_view_load_html)(view, html.as_ptr(), cstr("about:blank").as_ptr()) };
        }
        "size" => {
            // The window's scale as WebKit's page zoom (CSS pixels to the GUI's).
            let zoom = command["zoom"].as_f64().unwrap_or(1.0).clamp(0.5, 4.0);
            // SAFETY: the view of this thread.
            unsafe { (g.webkit_web_view_set_zoom_level)(view, zoom) };
            let (w, h) = (command["w"].as_i64().unwrap_or(800).clamp(16, 8192) as i32, command["h"].as_i64().unwrap_or(600).clamp(16, 8192) as i32);
            let (window, old) = {
                let mut s = state.borrow_mut();
                if (w, h) == (s.width, s.height) {
                    return;
                }
                s.width = w;
                s.height = h;
                s.damaged = true;
                (s.window, std::mem::replace(&mut s.surface, std::ptr::null_mut()))
            };
            // SAFETY: our old surface (no longer drawn into), the offscreen window and its child.
            unsafe {
                if !old.is_null() {
                    (g.cairo_surface_destroy)(old);
                }
                (g.gtk_widget_set_size_request)(view, w, h);
                (g.gtk_window_resize)(window, w, h);
            }
        }
        "eval" => {
            *next_eval += 1;
            let id = command["id"].as_u64().unwrap_or(*next_eval);
            let script = cstr(command["script"].as_str().unwrap_or_default());
            let settings = {
                let mut s = state.borrow_mut();
                s.evals.push(id);
                s.settings
            };
            set_bool(settings, "enable-javascript", true);
            let call = Box::into_raw(Box::new(EvalCall { state: state as *const RefCell<State> as gpointer, id, copy: false }));
            // SAFETY: the script is copied; the call box is freed by the callback.
            unsafe { (g.webkit_web_view_evaluate_javascript)(view, script.as_ptr(), -1, null(), null(), std::ptr::null_mut(), on_evaluated as *const c_void, call as gpointer) };
        }
        "copy" => {
            // The selection's text, read with the API's JavaScript (on only while it runs).
            let settings = {
                let mut s = state.borrow_mut();
                s.evals.push(u64::MAX);
                s.settings
            };
            set_bool(settings, "enable-javascript", true);
            let call = Box::into_raw(Box::new(EvalCall { state: state as *const RefCell<State> as gpointer, id: u64::MAX, copy: true }));
            let script = cstr("window.getSelection().toString()");
            // SAFETY: as for eval.
            unsafe { (g.webkit_web_view_evaluate_javascript)(view, script.as_ptr(), -1, null(), null(), std::ptr::null_mut(), on_evaluated as *const c_void, call as gpointer) };
        }
        "wheel" | "motion" | "button" | "key" => input(view, command),
        _ => {}
    }
}

/// Synthesizes a GDK event on the view (input the GUI received over it).
fn input(view: gpointer, command: &Value) {
    let g = gtk();
    let f = |k: &str| command[k].as_f64().unwrap_or(0.0);
    // SAFETY: events allocated by GDK, filled for their type, handed to the
    // view and freed (which releases the window and device references).
    unsafe {
        let window = (g.gtk_widget_get_window)(view);
        if window.is_null() {
            return;
        }
        let seat = (g.gdk_display_get_default_seat)((g.gdk_window_get_display)(window));
        let state = if command["shift"].as_bool() == Some(true) { GDK_SHIFT_MASK } else { 0 } | if command["ctrl"].as_bool() == Some(true) { GDK_CONTROL_MASK } else { 0 };
        let event = match command["cmd"].as_str().unwrap_or_default() {
            "wheel" => {
                let event = (g.gdk_event_new)(GDK_SCROLL);
                let e = &mut *(event as *mut EventScroll);
                e.window = (g.g_object_ref)(window);
                e.send_event = 1;
                e.x = f("x");
                e.y = f("y");
                e.state = state;
                e.direction = GDK_SCROLL_SMOOTH;
                e.delta_x = f("dx");
                e.delta_y = f("dy");
                e.is_stop = command["stop"].as_bool().unwrap_or(false) as u32;
                (g.gdk_event_set_device)(event, (g.gdk_seat_get_pointer)(seat));
                event
            }
            "motion" => {
                let event = (g.gdk_event_new)(GDK_MOTION_NOTIFY);
                let e = &mut *(event as *mut EventMotion);
                e.window = (g.g_object_ref)(window);
                e.send_event = 1;
                e.x = f("x");
                e.y = f("y");
                e.state = state | if command["held"].as_bool() == Some(true) { GDK_BUTTON1_MASK } else { 0 };
                (g.gdk_event_set_device)(event, (g.gdk_seat_get_pointer)(seat));
                event
            }
            "button" => {
                let pressed = command["pressed"].as_bool().unwrap_or(false);
                let event = (g.gdk_event_new)(if pressed { GDK_BUTTON_PRESS } else { GDK_BUTTON_RELEASE });
                let e = &mut *(event as *mut EventButton);
                e.window = (g.g_object_ref)(window);
                e.send_event = 1;
                e.x = f("x");
                e.y = f("y");
                e.button = command["button"].as_u64().unwrap_or(1) as u32;
                e.state = state | if pressed { 0 } else { GDK_BUTTON1_MASK };
                (g.gdk_event_set_device)(event, (g.gdk_seat_get_pointer)(seat));
                if pressed {
                    (g.gtk_widget_grab_focus)(view);
                }
                event
            }
            _ => {
                let pressed = command["pressed"].as_bool().unwrap_or(false);
                let Some(keyval) = command["keyval"].as_u64() else { return };
                let event = (g.gdk_event_new)(if pressed { GDK_KEY_PRESS } else { GDK_KEY_RELEASE });
                let e = &mut *(event as *mut EventKey);
                e.window = (g.g_object_ref)(window);
                e.send_event = 1;
                e.state = state;
                e.keyval = keyval as u32;
                (g.gdk_event_set_device)(event, (g.gdk_seat_get_keyboard)(seat));
                event
            }
        };
        (g.gtk_widget_event)(view, event);
        (g.gdk_event_free)(event);
    }
}
