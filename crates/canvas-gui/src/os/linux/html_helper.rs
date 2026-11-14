//! The HTML view's helper process (MAC-LINUX-PLAN §5.7, D9; the spike's
//! decision in PORT-STATUS): `atelier-gui` started again with
//! `ATELIER_HTML_VIEW=1` runs WebKitGTK 4.1 in `GtkOffscreenWindow`s
//! (no surface on screen; its own display connection, X11 or Wayland alike)
//! and talks to the GUI over its standard streams, both ways in records of a
//! kind byte and a little-endian length:
//!
//! - in: `J` a JSON command (`load`, `load_uri`, `unload`, `show`, `size`,
//!   `wheel`, `motion`, `button`, `key`, `eval`, `copy`, `print`), `R` the
//!   answer to a request of the notes scheme (a JSON head's length, the head
//!   `{id, status, reason, headers}`, the body);
//! - out: `J` a JSON event (`ready`, `failed`, `view_failed`, `loaded`,
//!   `request`, `link`, `fragment`, `blocked_navigation`, `blocked_request`,
//!   `blocked_window`, `reply`, `copied`, `printed`, …), `F` a frame (the
//!   view, width, height, straight RGBA rows).
//!
//! Two views, as WebView2's two on Windows (NOTEBOOK-TASKS §4): the plain one
//! never runs a page's scripts (reports, rendered Markdown, HTML notes);
//! HTML notes allowed to run theirs get the scripts view, made when first
//! needed in its own ephemeral context (its own website data manager:
//! nothing stored, nothing shared with the plain view), whose page becomes
//! `about:blank` whenever it is hidden. HTML notes come from the custom
//! scheme `canvas-notes://notes.canvas.invalid/` (WebKit lets no handler
//! serve https): every request of it goes to the GUI, which serves the
//! note's folder through the backend (`webpane::classify`) and answers 403
//! otherwise. A crash of GTK or WebKit (WebKit aborts when its bubblewrap
//! sandbox cannot start) ends this process only; the GUI then shows pages as
//! formatted text. The views are locked down in layers on top of the core's
//! sanitizer and the pages' Content-Security-Policy:
//!
//! - the plain view's JavaScript off (`enable-javascript`, and
//!   `enable-javascript-markup` removes script elements and handler
//!   attributes while parsing); the test hooks' `eval` turns the API's
//!   JavaScript on only while it runs;
//! - every http(s) and ws(s) request goes to a local proxy of this process
//!   that answers 403 (also WebKit's preconnects to a clicked link's host);
//!   `file:` and `ftp:` loads (and the plain view's `blob:`) are blocked by a
//!   content rule list;
//! - every navigation but the GUI's own load of the page is refused; a link
//!   the user clicks becomes a request to the canvas (which asks first, or
//!   navigates the app); no new windows, downloads cancelled, permissions
//!   denied, no context menu, no developer extras; ephemeral contexts;
//! - WebKit's bubblewrap sandbox when a probe shows bubblewrap can run here;
//! - *Print / PDF* prints the shown page to a PDF file the GUI named (GTK's
//!   file printer only: the helper loads no other print backend).

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::ptr::null;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::gtk::*;

/// The environment variable that makes the GUI binary this helper.
pub const ENV: &str = "ATELIER_HTML_VIEW";
/// The data home (the content rule lists' compiled store lives under it).
pub const HOME_ENV: &str = "ATELIER_HTML_VIEW_HOME";
/// The scheme HTML notes are served from.
pub const SCHEME: &str = "canvas-notes";

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

/// Which view: the plain one never runs a page's scripts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Plain = 0,
    Scripts = 1,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::Plain => "plain",
            Role::Scripts => "scripts",
        }
    }

    pub fn of(name: &str) -> Role {
        if name == "scripts" { Role::Scripts } else { Role::Plain }
    }
}

/// One WebKit view in its offscreen window.
struct View {
    view: gpointer,
    window: gpointer,
    settings: gpointer,
    surface: gpointer,
    /// The GUI's own next navigation: `""` a page given as a string
    /// (`about:blank` or `data:`), else that URI.
    expect: Option<String>,
    /// The load the GUI asked for last (reported when it finishes).
    load: u64,
    damaged: bool,
}

/// What the main loop and the signal handlers share (the GTK thread only).
struct State {
    views: [Option<View>; 2],
    /// The view whose frames go to the GUI.
    active: Role,
    out: Sender<Out>,
    width: i32,
    height: i32,
    zoom: f64,
    loads: u32,
    /// Pending `eval`s per view (the plain view's JavaScript is on only while one runs).
    evals: Vec<(u64, Role)>,
    /// Requests of the notes scheme waiting for the GUI's answer.
    requests: HashMap<u64, gpointer>,
    next_request: u64,
    proxy_port: u16,
    sandbox: bool,
    home: PathBuf,
    /// The PDF being printed (its path), until the operation finishes.
    printing: Option<(gpointer, String, String)>,
}

/// A record for the GUI.
enum Out {
    Event(Value),
    Frame(Role, u32, u32, Vec<u8>),
}

/// The data every handler of one view gets.
struct Handler {
    state: &'static RefCell<State>,
    role: Role,
}

/// Sends an event; the state is borrowed only for the moment it takes (GTK
/// and WebKit calls may run other handlers synchronously).
fn event(state: &RefCell<State>, value: Value) {
    let out = state.borrow().out.clone();
    let _ = out.send(Out::Event(value));
}

fn wake() {
    if let Some(gtk) = GTK.get() {
        // SAFETY: waking the default main context is thread safe.
        unsafe { (gtk.g_main_context_wakeup)(std::ptr::null_mut()) };
    }
}

/// A command from the GUI.
pub enum Command {
    Json(Value),
    /// An answer to a request of the notes scheme.
    Reply { id: u64, status: u32, reason: String, headers: String, body: Vec<u8> },
}

/// Reads one record (`None` at the end of the stream).
pub fn read_record(input: &mut impl Read) -> Option<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    input.read_exact(&mut head).ok()?;
    let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
    let mut body = vec![0u8; len];
    input.read_exact(&mut body).ok()?;
    Some((head[0], body))
}

/// Writes one record.
pub fn write_record(output: &mut impl Write, kind: u8, body: &[u8]) -> std::io::Result<()> {
    output.write_all(&[kind])?;
    output.write_all(&(body.len() as u32).to_le_bytes())?;
    output.write_all(body)?;
    output.flush()
}

/// A command record's meaning.
pub fn command_of_record(kind: u8, body: &[u8]) -> Option<Command> {
    match kind {
        b'J' => serde_json::from_slice(body).ok().map(Command::Json),
        b'R' if body.len() >= 4 => {
            let len = u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
            let head: Value = serde_json::from_slice(body.get(4..4 + len)?).ok()?;
            Some(Command::Reply {
                id: head["id"].as_u64()?,
                status: head["status"].as_u64().unwrap_or(500) as u32,
                reason: head["reason"].as_str().unwrap_or_default().to_owned(),
                headers: head["headers"].as_str().unwrap_or_default().to_owned(),
                body: body[4 + len..].to_vec(),
            })
        }
        _ => None,
    }
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
                Out::Frame(role, w, h, rgba) => {
                    let mut body = Vec::with_capacity(9 + rgba.len());
                    body.push(role as u8);
                    body.extend(w.to_le_bytes());
                    body.extend(h.to_le_bytes());
                    body.extend(rgba);
                    (b'F', body)
                }
            };
            if write_record(&mut stdout, kind, &body).is_err() {
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
            wake();
        }
    });
    Ok(port)
}

/// The plain view blocks `file:`, `blob:` and `ftp:`; the scripts view keeps
/// `blob:` (its policy lets a note's script make images and media of its own).
const RULES_PLAIN: &str = r#"[{"trigger":{"url-filter":"^file:"},"action":{"type":"block"}},{"trigger":{"url-filter":"^blob:"},"action":{"type":"block"}},{"trigger":{"url-filter":"^ftp:"},"action":{"type":"block"}}]"#;
const RULES_SCRIPTS: &str = r#"[{"trigger":{"url-filter":"^file:"},"action":{"type":"block"}},{"trigger":{"url-filter":"^ftp:"},"action":{"type":"block"}}]"#;

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

/// Compiles a content rule list into the home's store (once per version of it).
fn compile_filter(home: &std::path::Path, name: &str, rules: &str) -> Result<gpointer, String> {
    let g = gtk();
    let store_dir = home.join("webkit").join("content-rules");
    let _ = std::fs::create_dir_all(&store_dir);
    // SAFETY: WebKit calls on the GTK thread; the save completes (or times out)
    // before `saved` and the bytes go.
    unsafe {
        let store = (g.webkit_user_content_filter_store_new)(cstr(&store_dir.display().to_string()).as_ptr());
        let mut saved = FilterSaved { done: false, filter: std::ptr::null_mut(), error: String::new() };
        let bytes = (g.g_bytes_new)(rules.as_ptr() as *const c_void, rules.len());
        (g.webkit_user_content_filter_store_save)(store, cstr(name).as_ptr(), bytes, std::ptr::null_mut(), on_filter_saved as *const c_void, &mut saved as *mut FilterSaved as gpointer);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !saved.done && Instant::now() < deadline {
            (g.g_main_context_iteration)(std::ptr::null_mut(), 0);
            std::thread::sleep(Duration::from_millis(2));
        }
        (g.g_bytes_unref)(bytes);
        if saved.filter.is_null() {
            return Err(format!("the content rule list did not compile: {}", saved.error));
        }
        Ok(saved.filter)
    }
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

fn handler_of(data: gpointer) -> &'static Handler {
    // SAFETY: `data` is a leaked handler, one per view, handed to its handlers.
    unsafe { &*(data as *const Handler) }
}

unsafe extern "C" fn on_load_changed(view: gpointer, load_event: c_int, data: gpointer) {
    if load_event == WEBKIT_LOAD_FINISHED {
        let Handler { state, role } = handler_of(data);
        let (loads, load) = {
            let mut s = state.borrow_mut();
            s.loads += 1;
            let loads = s.loads;
            let load = s.views[*role as usize].as_mut().map(|v| {
                v.damaged = true;
                v.load
            });
            (loads, load.unwrap_or(0))
        };
        let (title, uri) = unsafe { (text((gtk().webkit_web_view_get_title)(view)), text((gtk().webkit_web_view_get_uri)(view))) };
        event(state, json!({ "event": "loaded", "view": role.name(), "load": load, "loads": loads, "title": title, "source": uri }));
    }
}

/// `uri` only moves to a fragment of the document at `current`.
fn same_document(current: &str, uri: &str) -> bool {
    let base = |u: &str| u.split_once('#').map_or(u, |(b, _)| b).to_owned();
    !current.is_empty() && uri.contains('#') && base(current) == base(uri)
}

/// The fragment names a place in the app (`#note:…`, `#todo:…`).
fn app_fragment(uri: &str) -> bool {
    uri.split_once('#').is_some_and(|(_, f)| {
        let f = f.to_ascii_lowercase();
        f.starts_with("note:") || f.starts_with("todo:")
    })
}

unsafe extern "C" fn on_decide_policy(view: gpointer, decision: gpointer, kind: c_int, data: gpointer) -> gboolean {
    if kind == WEBKIT_POLICY_DECISION_TYPE_RESPONSE {
        return 0;
    }
    let g = gtk();
    let Handler { state, role } = handler_of(data);
    // SAFETY: a navigation or new-window decision of this view.
    unsafe {
        let action = (g.webkit_navigation_policy_decision_get_navigation_action)(decision);
        let uri = text((g.webkit_uri_request_get_uri)((g.webkit_navigation_action_get_request)(action)));
        let current = text((g.webkit_web_view_get_uri)(view));
        let navigation = kind == WEBKIT_POLICY_DECISION_TYPE_NAVIGATION_ACTION;
        let expected = {
            let mut s = state.borrow_mut();
            let v = s.views[*role as usize].as_mut();
            let hit = v.as_ref().and_then(|v| v.expect.as_ref()).is_some_and(|e| {
                if e.is_empty() { uri.starts_with("about:blank") || uri.starts_with("data:") } else { uri == *e || uri.split_once('#').is_some_and(|(b, _)| b == e) }
            });
            if hit && navigation && let Some(v) = v {
                v.expect = None;
            }
            hit && navigation
        };
        if expected {
            (g.webkit_policy_decision_use)(decision);
            return 1;
        }
        if navigation && same_document(&current, &uri) {
            if app_fragment(&uri) {
                // A place in the app: the GUI decides (in the scripts view once per click into it).
                (g.webkit_policy_decision_ignore)(decision);
                event(state, json!({ "event": "fragment", "view": role.name(), "url": uri }));
            } else {
                // An in-page anchor scrolls.
                (g.webkit_policy_decision_use)(decision);
            }
            return 1;
        }
        let clicked = (g.webkit_navigation_action_get_navigation_type)(action) == WEBKIT_NAVIGATION_TYPE_LINK_CLICKED && (g.webkit_navigation_action_is_user_gesture)(action) != 0;
        (g.webkit_policy_decision_ignore)(decision);
        if clicked {
            event(state, json!({ "event": "link", "view": role.name(), "url": uri }));
        } else {
            event(state, json!({ "event": "blocked_navigation", "view": role.name(), "url": uri }));
        }
    }
    1
}

unsafe extern "C" fn on_create(_view: gpointer, action: gpointer, data: gpointer) -> gpointer {
    let g = gtk();
    let Handler { state, role } = handler_of(data);
    // SAFETY: the navigation action the signal hands over.
    let (uri, user) = unsafe { (text((g.webkit_uri_request_get_uri)((g.webkit_navigation_action_get_request)(action))), (g.webkit_navigation_action_is_user_gesture)(action) != 0) };
    event(state, json!({ "event": "blocked_window", "view": role.name(), "url": uri }));
    if user {
        // A link for a new window the user clicked: the canvas asks (no window opens).
        event(state, json!({ "event": "link", "view": role.name(), "url": uri }));
    }
    std::ptr::null_mut()
}

unsafe extern "C" fn on_permission(_view: gpointer, request: gpointer, data: gpointer) -> gboolean {
    unsafe { (gtk().webkit_permission_request_deny)(request) };
    event(handler_of(data).state, json!({ "event": "denied_permission" }));
    1
}

unsafe extern "C" fn on_context_menu(_view: gpointer, _menu: gpointer, _event: gpointer, _hit: gpointer, _data: gpointer) -> gboolean {
    1
}

unsafe extern "C" fn on_download(_context: gpointer, download: gpointer, data: gpointer) {
    unsafe { (gtk().webkit_download_cancel)(download) };
    event(handler_of(data).state, json!({ "event": "blocked_download" }));
}

unsafe extern "C" fn on_terminated(_view: gpointer, reason: c_int, data: gpointer) {
    let Handler { state, role } = handler_of(data);
    let why = format!("WebKit's web process ended ({reason})");
    match role {
        Role::Plain => event(state, json!({ "event": "failed", "why": why })),
        Role::Scripts => event(state, json!({ "event": "view_failed", "view": "scripts", "why": why })),
    }
}

unsafe extern "C" fn on_damage(_widget: gpointer, _event: gpointer, data: gpointer) -> gboolean {
    let Handler { state, role } = handler_of(data);
    if let Some(v) = state.borrow_mut().views[*role as usize].as_mut() {
        v.damaged = true;
    }
    0
}

/// A request of the notes scheme: handed to the GUI, answered by `Reply`.
unsafe extern "C" fn on_scheme(request: gpointer, data: gpointer) {
    let g = gtk();
    let Handler { state, role } = handler_of(data);
    // SAFETY: the request the callback hands over, kept (a reference) until answered.
    let (uri, method) = unsafe {
        (g.g_object_ref)(request);
        let method = text((g.webkit_uri_scheme_request_get_http_method)(request));
        (text((g.webkit_uri_scheme_request_get_uri)(request)), if method.is_empty() { "GET".to_owned() } else { method })
    };
    let id = {
        let mut s = state.borrow_mut();
        s.next_request += 1;
        let id = s.next_request;
        s.requests.insert(id, request);
        id
    };
    event(state, json!({ "event": "request", "id": id, "view": role.name(), "uri": uri, "method": method }));
}

/// Answers request `id` of the notes scheme (gone already: nothing to do).
fn answer(state: &'static RefCell<State>, id: u64, status: u32, reason: &str, headers: &str, body: &[u8]) {
    let g = gtk();
    let Some(request) = state.borrow_mut().requests.remove(&id) else { return };
    // SAFETY: the kept request and objects made here; the headers go to the
    // response (transfer full), the rest is released after the answer.
    unsafe {
        let bytes = (g.g_bytes_new)(body.as_ptr() as *const c_void, body.len());
        let stream = (g.g_memory_input_stream_new_from_bytes)(bytes);
        (g.g_bytes_unref)(bytes);
        let response = (g.webkit_uri_scheme_response_new)(stream, body.len() as i64);
        (g.webkit_uri_scheme_response_set_status)(response, status, cstr(reason).as_ptr());
        let fields = (g.soup_message_headers_new)(SOUP_MESSAGE_HEADERS_RESPONSE);
        for line in headers.split("\r\n") {
            let Some((name, value)) = line.split_once(':') else { continue };
            let (name, value) = (name.trim(), value.trim());
            if name.eq_ignore_ascii_case("content-type") {
                (g.webkit_uri_scheme_response_set_content_type)(response, cstr(value).as_ptr());
            }
            (g.soup_message_headers_append)(fields, cstr(name).as_ptr(), cstr(value).as_ptr());
        }
        (g.webkit_uri_scheme_response_set_http_headers)(response, fields);
        (g.webkit_uri_scheme_request_finish_with_response)(request, response);
        (g.g_object_unref)(response);
        (g.g_object_unref)(stream);
        (g.g_object_unref)(request);
    }
}

struct EvalCall {
    state: &'static RefCell<State>,
    id: u64,
    role: Role,
    /// A copy of the selection rather than a hook's evaluation.
    copy: bool,
}

unsafe extern "C" fn on_evaluated(view: gpointer, result: gpointer, data: gpointer) {
    let call = unsafe { Box::from_raw(data as *mut EvalCall) };
    let g = gtk();
    let mut error: *mut GError = std::ptr::null_mut();
    let value = unsafe { (g.webkit_web_view_evaluate_javascript_finish)(view, result, &mut error) };
    let message = (!error.is_null()).then(|| {
        let message = text(unsafe { (*error).message });
        unsafe { (g.g_error_free)(error) };
        message
    });
    let result = (!value.is_null()).then(|| {
        let raw = unsafe { (g.jsc_value_to_string)(value) };
        let result = text(raw);
        unsafe {
            (g.g_free)(raw as gpointer);
            (g.g_object_unref)(value);
        }
        result
    });
    let reply = if call.copy {
        json!({ "event": "copied", "text": if message.is_none() { result.unwrap_or_default() } else { String::new() } })
    } else if let Some(message) = message {
        json!({ "event": "reply", "id": call.id, "ok": false, "error": message })
    } else {
        json!({ "event": "reply", "id": call.id, "ok": true, "result": result })
    };
    let (settings, done) = {
        let mut s = call.state.borrow_mut();
        s.evals.retain(|(id, role)| (*id, *role) != (call.id, call.role));
        let done = !s.evals.iter().any(|(_, role)| *role == call.role);
        (s.views[call.role as usize].as_ref().map(|v| v.settings), done)
    };
    // The plain view's own JavaScript stays off: the API's is on only while evaluating.
    if done && call.role == Role::Plain && let Some(settings) = settings {
        set_bool(settings, "enable-javascript", false);
    }
    event(call.state, reply);
}

/// Makes a view: its own ephemeral context (website data manager), the deny
/// proxy, the notes scheme, its content rule list, settings and signals.
fn make_view(state: &'static RefCell<State>, role: Role) -> Result<View, String> {
    let g = gtk();
    let (port, sandbox, home, width, height, zoom) = {
        let s = state.borrow();
        (s.proxy_port, s.sandbox, s.home.clone(), s.width, s.height, s.zoom)
    };
    let (name, rules) = match role {
        Role::Plain => ("canvas-report", RULES_PLAIN),
        Role::Scripts => ("canvas-scripts", RULES_SCRIPTS),
    };
    let filter = compile_filter(&home, name, rules)?;
    let data = Box::leak(Box::new(Handler { state, role })) as *const Handler as gpointer;
    // SAFETY: plain WebKitGTK calls on the GTK thread with valid arguments.
    unsafe {
        let context = (g.webkit_web_context_new_ephemeral)();
        if sandbox {
            (g.webkit_web_context_set_sandbox_enabled)(context, 1);
        }
        (g.webkit_web_context_set_cache_model)(context, WEBKIT_CACHE_MODEL_DOCUMENT_VIEWER);
        let proxy = cstr(&format!("http://127.0.0.1:{port}"));
        let settings = (g.webkit_network_proxy_settings_new)(proxy.as_ptr(), null());
        (g.webkit_website_data_manager_set_network_proxy_settings)((g.webkit_web_context_get_website_data_manager)(context), WEBKIT_NETWORK_PROXY_MODE_CUSTOM, settings);
        (g.webkit_network_proxy_settings_free)(settings);
        // The notes origin: answered by the GUI (the note's folder), a secure
        // scheme whose fonts and module scripts a sandboxed page may load.
        let scheme = cstr(SCHEME);
        (g.webkit_web_context_register_uri_scheme)(context, scheme.as_ptr(), on_scheme as *const c_void, data, std::ptr::null_mut());
        let security = (g.webkit_web_context_get_security_manager)(context);
        (g.webkit_security_manager_register_uri_scheme_as_secure)(security, scheme.as_ptr());
        (g.webkit_security_manager_register_uri_scheme_as_cors_enabled)(security, scheme.as_ptr());

        let content = (g.webkit_user_content_manager_new)();
        (g.webkit_user_content_manager_add_filter)(content, filter);

        let settings = (g.webkit_settings_new)();
        let scripts = role == Role::Scripts;
        for (name, value) in [
            ("enable-javascript", scripts),
            ("enable-javascript-markup", scripts),
            ("javascript-can-open-windows-automatically", false),
            ("javascript-can-access-clipboard", false),
            ("enable-developer-extras", false),
            ("enable-page-cache", false),
            ("enable-html5-local-storage", false),
            ("enable-html5-database", false),
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
        if view.is_null() {
            return Err("WebKit made no view".into());
        }
        (g.webkit_web_view_set_zoom_level)(view, zoom);
        let window = (g.gtk_offscreen_window_new)();
        (g.gtk_window_set_default_size)(window, width, height);
        (g.gtk_widget_set_size_request)(view, width, height);
        (g.gtk_container_add)(window, view);
        connect(view, "load-changed", on_load_changed as *const c_void, data);
        connect(view, "decide-policy", on_decide_policy as *const c_void, data);
        connect(view, "create", on_create as *const c_void, data);
        connect(view, "permission-request", on_permission as *const c_void, data);
        connect(view, "context-menu", on_context_menu as *const c_void, data);
        connect(view, "web-process-terminated", on_terminated as *const c_void, data);
        connect(context, "download-started", on_download as *const c_void, data);
        connect(window, "damage-event", on_damage as *const c_void, data);
        (g.gtk_widget_show_all)(window);
        Ok(View { view, window, settings, surface: std::ptr::null_mut(), expect: None, load: 0, damaged: true })
    }
}

/// The view for `role`, made when first needed (`None`: it could not be;
/// the GUI was told).
fn view_of(state: &'static RefCell<State>, role: Role) -> Option<gpointer> {
    if let Some(v) = &state.borrow().views[role as usize] {
        return Some(v.view);
    }
    match make_view(state, role) {
        Ok(view) => {
            let handle = view.view;
            state.borrow_mut().views[role as usize] = Some(view);
            Some(handle)
        }
        Err(why) => {
            event(state, json!({ "event": "view_failed", "view": role.name(), "why": why }));
            None
        }
    }
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
    let state: &'static RefCell<State> = Box::leak(Box::new(RefCell::new(State {
        views: [None, None],
        active: Role::Plain,
        out: out.clone(),
        width: 800,
        height: 600,
        zoom: 1.0,
        loads: 0,
        evals: Vec::new(),
        requests: HashMap::new(),
        next_request: 0,
        proxy_port: port,
        sandbox,
        home,
        printing: None,
    })));
    let plain = make_view(state, Role::Plain)?;
    state.borrow_mut().views[0] = Some(plain);
    event(state, json!({ "event": "ready", "webkit": version, "sandbox": sandbox, "proxy_port": port }));

    // Commands from the GUI, read on a thread that wakes the main loop.
    let (tx, rx) = channel::<Option<Command>>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        while let Some((kind, body)) = read_record(&mut stdin) {
            if let Some(command) = command_of_record(kind, &body) {
                let _ = tx.send(Some(command));
                wake();
            }
        }
        let _ = tx.send(None);
        wake();
    });
    main_loop(state, rx);
    Ok(())
}

fn main_loop(state: &'static RefCell<State>, commands: Receiver<Option<Command>>) {
    let g = gtk();
    let mut last_frame = Instant::now().checked_sub(Duration::from_secs(1)).unwrap_or_else(Instant::now);
    let mut next_eval = 0u64;
    loop {
        loop {
            match commands.try_recv() {
                Ok(Some(Command::Json(command))) => command_of(state, &command, &mut next_eval),
                Ok(Some(Command::Reply { id, status, reason, headers, body })) => answer(state, id, status, &reason, &headers, &body),
                // The GUI went away: so does the helper.
                Ok(None) => return,
                Err(_) => break,
            }
        }
        let active = state.borrow().active;
        let damaged = state.borrow().views[active as usize].as_ref().is_some_and(|v| v.damaged);
        if damaged && last_frame.elapsed() >= Duration::from_millis(16) {
            if let Some(v) = state.borrow_mut().views[active as usize].as_mut() {
                v.damaged = false;
            }
            send_frame(state, active);
            last_frame = Instant::now();
        }
        // SAFETY: the GTK thread's main loop.
        unsafe {
            if damaged {
                // A frame is due soon: do not block.
                (g.g_main_context_iteration)(std::ptr::null_mut(), 0);
                std::thread::sleep(Duration::from_millis(4));
            } else {
                (g.g_main_context_iteration)(std::ptr::null_mut(), 1);
            }
        }
    }
}

/// Draws a view into an image surface and sends it as straight RGBA.
fn send_frame(state: &'static RefCell<State>, role: Role) {
    let g = gtk();
    let (view, w, h, mut surface, out) = {
        let s = state.borrow();
        let Some(v) = s.views[role as usize].as_ref() else { return };
        (v.view, s.width.max(1), s.height.max(1), v.surface, s.out.clone())
    };
    // SAFETY: cairo and GTK calls on the GTK thread; the surface is ours.
    unsafe {
        if surface.is_null() {
            // CAIRO_FORMAT_ARGB32.
            surface = (g.cairo_image_surface_create)(0, w, h);
            if let Some(v) = state.borrow_mut().views[role as usize].as_mut() {
                v.surface = surface;
            }
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
        let _ = out.send(Out::Frame(role, sw as u32, sh as u32, rgba));
    }
}

/// Runs script in `role`'s view (the plain view's JavaScript on while it runs).
fn evaluate(state: &'static RefCell<State>, role: Role, id: u64, script: &str, copy: bool) {
    let g = gtk();
    let Some((view, settings)) = state.borrow().views[role as usize].as_ref().map(|v| (v.view, v.settings)) else {
        if !copy {
            event(state, json!({ "event": "reply", "id": id, "ok": false, "error": "no HTML view" }));
        }
        return;
    };
    state.borrow_mut().evals.push((id, role));
    if role == Role::Plain {
        set_bool(settings, "enable-javascript", true);
    }
    let call = Box::into_raw(Box::new(EvalCall { state, id, role, copy }));
    let script = cstr(script);
    // SAFETY: the script is copied; the call box is freed by the callback.
    unsafe { (g.webkit_web_view_evaluate_javascript)(view, script.as_ptr(), -1, null(), null(), std::ptr::null_mut(), on_evaluated as *const c_void, call as gpointer) };
}

fn command_of(state: &'static RefCell<State>, command: &Value, next_eval: &mut u64) {
    let g = gtk();
    let role = Role::of(command["view"].as_str().unwrap_or("plain"));
    match command["cmd"].as_str().unwrap_or_default() {
        "load" => {
            let Some(view) = view_of(state, role) else { return };
            let html = cstr(command["html"].as_str().unwrap_or_default());
            if let Some(v) = state.borrow_mut().views[role as usize].as_mut() {
                (v.expect, v.load) = (Some(String::new()), command["load"].as_u64().unwrap_or(0));
            }
            // SAFETY: the page is copied by WebKit.
            unsafe { (g.webkit_web_view_load_html)(view, html.as_ptr(), cstr("about:blank").as_ptr()) };
        }
        "load_uri" => {
            let Some(view) = view_of(state, role) else { return };
            let uri = command["uri"].as_str().unwrap_or_default().to_owned();
            if let Some(v) = state.borrow_mut().views[role as usize].as_mut() {
                (v.expect, v.load) = (Some(uri.clone()), command["load"].as_u64().unwrap_or(0));
            }
            // SAFETY: the URI is copied by WebKit.
            unsafe { (g.webkit_web_view_load_uri)(view, cstr(&uri).as_ptr()) };
        }
        "unload" => {
            // A hidden scripts view keeps no page (its scripts stop with it).
            let Some(view) = state.borrow().views[role as usize].as_ref().map(|v| v.view) else { return };
            if let Some(v) = state.borrow_mut().views[role as usize].as_mut() {
                (v.expect, v.load) = (Some(String::new()), 0);
            }
            // SAFETY: a view of this thread.
            unsafe { (g.webkit_web_view_load_uri)(view, cstr("about:blank").as_ptr()) };
        }
        "show" => {
            let mut s = state.borrow_mut();
            s.active = role;
            if let Some(v) = s.views[role as usize].as_mut() {
                v.damaged = true;
            }
        }
        "size" => {
            // The window's scale as WebKit's page zoom (CSS pixels to the GUI's).
            let zoom = command["zoom"].as_f64().unwrap_or(1.0).clamp(0.5, 4.0);
            let (w, h) = (command["w"].as_i64().unwrap_or(800).clamp(16, 8192) as i32, command["h"].as_i64().unwrap_or(600).clamp(16, 8192) as i32);
            let views: Vec<(gpointer, gpointer, gpointer)> = {
                let mut s = state.borrow_mut();
                let resized = (w, h) != (s.width, s.height);
                (s.width, s.height, s.zoom) = (w, h, zoom);
                s.views
                    .iter_mut()
                    .flatten()
                    .map(|v| {
                        v.damaged = true;
                        let old = if resized { std::mem::replace(&mut v.surface, std::ptr::null_mut()) } else { std::ptr::null_mut() };
                        (v.view, v.window, old)
                    })
                    .collect()
            };
            // SAFETY: our old surfaces (no longer drawn into), the offscreen windows and their views.
            unsafe {
                for (view, window, old) in views {
                    (g.webkit_web_view_set_zoom_level)(view, zoom);
                    if !old.is_null() {
                        (g.cairo_surface_destroy)(old);
                    }
                    (g.gtk_widget_set_size_request)(view, w, h);
                    (g.gtk_window_resize)(window, w, h);
                }
            }
        }
        "eval" => {
            *next_eval += 1;
            let id = command["id"].as_u64().unwrap_or(*next_eval);
            let active = state.borrow().active;
            evaluate(state, active, id, command["script"].as_str().unwrap_or_default(), false);
        }
        "copy" => {
            // The selection's text (the plain view's API JavaScript on only while it runs).
            let active = state.borrow().active;
            evaluate(state, active, u64::MAX, "window.getSelection().toString()", true);
        }
        "print" => print(state, command["path"].as_str().unwrap_or_default()),
        "wheel" | "motion" | "button" | "key" => {
            let s = state.borrow();
            if let Some(view) = s.views[s.active as usize].as_ref().map(|v| v.view) {
                drop(s);
                input(view, command);
            }
        }
        _ => {}
    }
}

unsafe extern "C" fn on_printer(printer: gpointer, data: gpointer) -> gboolean {
    let found = unsafe { &mut *(data as *mut String) };
    let g = gtk();
    // SAFETY: a printer GTK lists.
    unsafe {
        if found.is_empty() && (g.gtk_printer_is_virtual)(printer) != 0 && (g.gtk_printer_accepts_pdf)(printer) != 0 {
            *found = text((g.gtk_printer_get_name)(printer));
        }
    }
    // Keep listing (GTK stops when this answers true).
    0
}

unsafe extern "C" fn on_print_failed(_operation: gpointer, error: *mut GError, data: gpointer) {
    let state = handler_of(data).state;
    if let Some((_, _, why)) = state.borrow_mut().printing.as_mut() {
        *why = if error.is_null() { "printing failed".into() } else { text(unsafe { (*error).message }) };
    }
}

unsafe extern "C" fn on_print_finished(_operation: gpointer, data: gpointer) {
    let state = handler_of(data).state;
    let Some((operation, path, why)) = state.borrow_mut().printing.take() else { return };
    // SAFETY: the operation made in `print`, done now.
    unsafe { (gtk().g_object_unref)(operation) };
    event(state, json!({ "event": "printed", "path": path, "ok": why.is_empty(), "error": why }));
}

/// Prints the shown page to `path` as a PDF through GTK's file printer
/// (the helper loads no other print backend: `GTK_PRINT_BACKENDS=file`).
fn print(state: &'static RefCell<State>, path: &str) {
    let g = gtk();
    let fail = |why: &str| event(state, json!({ "event": "printed", "path": path, "ok": false, "error": why }));
    if state.borrow().printing.is_some() {
        return fail("a PDF is being saved already");
    }
    let active = state.borrow().active;
    let Some(view) = state.borrow().views[active as usize].as_ref().map(|v| v.view) else { return fail("no page is shown") };
    let mut printer = String::new();
    // SAFETY: GTK calls on the GTK thread; the listing completes (wait) before
    // `printer` goes; the URI GLib made is freed after use.
    unsafe {
        (g.gtk_enumerate_printers)(on_printer as *const c_void, &mut printer as *mut String as gpointer, std::ptr::null_mut(), 1);
        if printer.is_empty() {
            return fail("GTK's file printer is not available");
        }
        let mut error: *mut GError = std::ptr::null_mut();
        let uri = (g.g_filename_to_uri)(cstr(path).as_ptr(), null(), &mut error);
        if uri.is_null() {
            let why = if error.is_null() { "not a file name".into() } else { text((*error).message) };
            if !error.is_null() {
                (g.g_error_free)(error);
            }
            return fail(&why);
        }
        let settings = (g.gtk_print_settings_new)();
        (g.gtk_print_settings_set_printer)(settings, cstr(&printer).as_ptr());
        (g.gtk_print_settings_set)(settings, cstr("output-file-format").as_ptr(), cstr("pdf").as_ptr());
        (g.gtk_print_settings_set)(settings, cstr("output-uri").as_ptr(), uri);
        (g.g_free)(uri as gpointer);
        let operation = (g.webkit_print_operation_new)(view);
        (g.webkit_print_operation_set_print_settings)(operation, settings);
        (g.g_object_unref)(settings);
        let data = Box::leak(Box::new(Handler { state, role: active })) as *const Handler as gpointer;
        connect(operation, "failed", on_print_failed as *const c_void, data);
        connect(operation, "finished", on_print_finished as *const c_void, data);
        state.borrow_mut().printing = Some((operation, path.to_owned(), String::new()));
        (g.webkit_print_operation_print)(operation);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_carry_commands_and_replies() {
        let mut wire = Vec::new();
        write_record(&mut wire, b'J', br#"{"cmd":"show","view":"scripts"}"#).unwrap();
        let head = br#"{"id":7,"status":200,"reason":"OK","headers":"Content-Type: text/css"}"#;
        let mut body = (head.len() as u32).to_le_bytes().to_vec();
        body.extend(head);
        body.extend(b"h1{}");
        write_record(&mut wire, b'R', &body).unwrap();
        let mut input = wire.as_slice();
        let (kind, body) = read_record(&mut input).unwrap();
        assert!(matches!(command_of_record(kind, &body), Some(Command::Json(v)) if v["view"] == "scripts"));
        let (kind, body) = read_record(&mut input).unwrap();
        match command_of_record(kind, &body) {
            Some(Command::Reply { id, status, reason, headers, body }) => {
                assert_eq!((id, status, reason.as_str(), headers.as_str(), body.as_slice()), (7, 200, "OK", "Content-Type: text/css", b"h1{}".as_slice()));
            }
            _ => panic!("a reply"),
        }
        assert!(read_record(&mut input).is_none());
    }

    #[test]
    fn fragments_and_app_links() {
        let page = "canvas-notes://notes.canvas.invalid/a/page.html";
        assert!(same_document(page, &format!("{page}#top")));
        assert!(same_document("about:blank", "about:blank#note:plan"));
        assert!(!same_document(page, "canvas-notes://notes.canvas.invalid/a/other.html#x"));
        assert!(!same_document("", "about:blank#x"));
        assert!(app_fragment(&format!("{page}#todo:PAR-001")) && app_fragment("about:blank#Note:Plan"));
        assert!(!app_fragment(&format!("{page}#top")) && !app_fragment(page));
        assert_eq!((Role::of("scripts"), Role::of("plain"), Role::of("x")), (Role::Scripts, Role::Plain, Role::Plain));
    }
}
