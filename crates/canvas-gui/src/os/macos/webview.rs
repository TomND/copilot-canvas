//! The GUI's HTML view on macOS (MAC-LINUX-PLAN D9, §5.7; docs/NOTEBOOK-TASKS.md
//! §4): reports, rendered Markdown notes and HTML notes in WKWebViews, child
//! views of the canvas window's content view over the pane. Agent-authored
//! HTML is data, never code, and the views are locked down as the iPhone
//! app's report view (`ReportWebView.swift`) and WebView2 on Windows:
//!
//! - two views: the plain one never runs a page's JavaScript
//!   (`allowsContentJavaScript` off for good: reports, rendered Markdown,
//!   HTML notes without scripts); HTML notes allowed to run theirs get a
//!   second one with its own non-persistent data store, whose page lives only
//!   while it is shown (`about:blank` when hidden). In both: no new windows,
//!   link previews, magnification, back/forward gestures or Web Inspector,
//!   no fraudulent-website lookups, media only on a user action, camera and
//!   microphone denied;
//! - a content rule list (compiled into `<home>/webkit-rules`) blocks every
//!   load with an address: `http(s)`, `ws(s)`, `ftp`, `file`, `blob`. The
//!   views are never created without it;
//! - HTML notes come from `canvas-notes://notes.canvas.invalid/` (a
//!   WKWebView answers only schemes of its own, so not the `https:` origin of
//!   Windows; `report_html::notes::SCHEME_ORIGIN`), mapped to the note's
//!   explorer root (`webpane::file_for`: nothing outside it) and of that only
//!   the note's own folder and the folders below it (`webpane::classify`):
//!   a `WKURLSchemeHandler` reads each file through the backend on a worker
//!   thread (also remotely; at most `notebook::MAX_BYTES`, at most
//!   `webpane::MAX_PENDING` waiting, the rest 503) and answers on the main
//!   thread with the note's policy and `nosniff` (`webpane::reply`, the
//!   policy's origin named as the scheme's). `GET` only; the note itself is
//!   sanitized unless its scripts may run. A page that is replaced has its
//!   waiting requests answered at once and their reads skipped;
//! - every navigation but the canvas's own load of the page is cancelled; a
//!   link the user clicks becomes a request to the canvas: http(s)/mailto
//!   asks before the browser, `#note:` / `#todo:` and links to other files
//!   under the note's root navigate the app (`webpane::link_action`);
//!   in-page `#fragment`s scroll; in the scripts view an app link counts
//!   only once per click into the page (`webpane::AppLinks`);
//! - the app's keys (⌘1/2/3, ⌘P, ⌘[ / ⌘], ⌘E, ⌘W, ⌘S, ⌘F, ⌘N, ⇧⌘[ / ⇧⌘],
//!   ⌃⌘N, F6, Ctrl+], Esc) go to the canvas, the rest stays in the page
//!   (⌘C copies its selection through the Edit menu);
//! - *Print / PDF* saves the page as a PDF in Downloads under a name no file
//!   has (`webpane::pdf_path`: never over a file).
//!
//! The views take the keyboard only after a click into them; Esc there closes
//! a report or gives the keyboard back to the canvas.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use block2::RcBlock;
use canvas_core::input::{KeyCode, KeyEvent, KeySource, Modifiers};
use canvas_core::report_html::notes::{ORIGIN, SCHEME, SCHEME_ORIGIN};
use canvas_core::rpc::Backend;
use canvas_core::spaces::{NavTarget, Space};
use canvas_core::view::{HtmlPane, HtmlSource};
use crossbeam_channel::{Receiver, Sender};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{ClassType, DeclaredClass, declare_class, msg_send, msg_send_id, mutability, sel};
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSColor, NSEvent, NSEventModifierFlags, NSImage, NSResponder, NSView};
use objc2_foundation::{MainThreadMarker, NSData, NSDictionary, NSError, NSHTTPURLResponse, NSNumber, NSPoint, NSRect, NSSize, NSString, NSURL, NSURLRequest};
use objc2_web_kit::{
    WKAudiovisualMediaTypes, WKContentRuleList, WKContentRuleListStore, WKContentWorld, WKFrameInfo, WKMediaCaptureType, WKNavigation, WKNavigationAction, WKNavigationActionPolicy,
    WKNavigationDelegate, WKNavigationType, WKPermissionDecision, WKScriptMessage, WKScriptMessageHandler, WKSecurityOrigin, WKUIDelegate, WKURLSchemeHandler, WKURLSchemeTask,
    WKUserContentController, WKUserScript, WKUserScriptInjectionTime, WKWebView, WKWebViewConfiguration, WKWebsiteDataStore, WKWindowFeatures,
};
use serde_json::{Value, json};

use crate::webpane::{self, AppLinks, Expect, LinkAction, Reply, Resource, Serving};

/// Loads with an address are blocked; `data:`, the page's own `about:blank`
/// and the notes scheme are not (the iPhone app's list).
const RULES: &str = r#"[
  {"trigger": {"url-filter": "^https?:"}, "action": {"type": "block"}},
  {"trigger": {"url-filter": "^wss?:"}, "action": {"type": "block"}},
  {"trigger": {"url-filter": "^ftp:"}, "action": {"type": "block"}},
  {"trigger": {"url-filter": "^file:"}, "action": {"type": "block"}},
  {"trigger": {"url-filter": "^blob:"}, "action": {"type": "block"}}
]"#;
const RULES_ID: &str = "copilot-canvas-reports-v1";

/// Counts refused loads (in the canvas's own world: the page sees nothing).
const COUNTER: &str = r#"(() => {
  window.addEventListener('error', (e) => {
    const t = e.target;
    if (!t || t === window) return;
    const u = t.currentSrc || t.src || t.href;
    if (u && !String(u).startsWith('data:') && !String(u).startsWith('canvas-notes:')) {
      try { window.webkit.messageHandlers.blocked.postMessage(String(u)); } catch (_) {}
    }
  }, true);
})();"#;
const WORLD: &str = "CopilotCanvasLockdown";
/// The window's background under the page (the canvas's).
const BACKGROUND: [f64; 3] = [13.0 / 255.0, 17.0 / 255.0, 23.0 / 255.0];
/// `kVK_Escape`, `kVK_F6`, `kVK_Tab`, `kVK_LeftArrow`, `kVK_RightArrow`.
const VK_ESCAPE: u16 = 53;
const VK_F6: u16 = 97;
const VK_TAB: u16 = 48;
const VK_LEFT: u16 = 123;
const VK_RIGHT: u16 = 124;
/// Threads reading notes' files.
const READERS: usize = 4;

/// What happened in the views that the window acts on.
pub enum WebEvent {
    Ready,
    Failed(String),
    /// The user clicked an http(s)/mailto link (the canvas asks before opening it).
    Link(String),
    /// Esc in the view: a report closes; else the keyboard goes back to the canvas.
    Escape,
    /// A global key pressed in the page, for the canvas.
    Key(KeyEvent),
    /// A `#note:` / `#todo:` link or a link to another file of the note's folder.
    Navigate(Space, NavTarget),
    /// Something to tell the user (printing); `true`: an error.
    Notice(String, bool),
}

#[derive(Clone, Debug, Default, PartialEq)]
enum Phase {
    #[default]
    Idle,
    Creating,
    Ready,
    Failed(String),
}

fn phase_text(phase: &Phase) -> String {
    match phase {
        Phase::Idle => "idle".to_owned(),
        Phase::Creating => "creating".into(),
        Phase::Ready => "ready".into(),
        Phase::Failed(why) => format!("failed: {why}"),
    }
}

/// Which view: the plain one (never scripts) or the scripts one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Plain = 0,
    Scripts = 1,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Role::Plain => "plain",
            Role::Scripts => "scripts",
        }
    }

    fn other(self) -> Role {
        match self {
            Role::Plain => Role::Scripts,
            Role::Scripts => Role::Plain,
        }
    }
}

/// A pane's id and what its view serves.
type Shown = (String, Option<Serving>);

/// One view.
#[derive(Default)]
struct Slot {
    phase: Phase,
    view: Option<Retained<ReportView>>,
    /// The canvas's own next navigation (allowed once).
    expect: Option<Expect>,
    /// What the view serves from the notes scheme (an HTML note).
    serving: Option<Serving>,
    shown: Option<Shown>,
    /// The page followed an app link: load it again when shown again.
    stale: bool,
    visible: bool,
    frame: Option<[f64; 4]>,
    bounds: Option<[i32; 4]>,
    /// A load of the canvas's is under way.
    loading: bool,
    links: AppLinks,
    /// Bumped when the page is replaced: reads of the old one are skipped.
    generation: Arc<AtomicU64>,
    /// What the view showed after its latest load (PNG), for window captures.
    snapshot: Option<Vec<u8>>,
}

/// A request of the notes scheme waiting for its file.
struct Pending {
    role: Role,
    task: Retained<ProtocolObject<dyn WKURLSchemeTask>>,
    url: Retained<NSURL>,
}

/// A file read for request `id`.
type Read = (u64, Reply);

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
struct State {
    /// The HTML view as a whole (its rule list, its plain view).
    phase: Phase,
    rules: Option<Retained<WKContentRuleList>>,
    slots: [Slot; 2],
    delegate: Option<Retained<Delegate>>,
    scheme: Option<Retained<SchemeHandler>>,
    events: Vec<WebEvent>,
    backend: Option<Arc<dyn Backend>>,
    pending: HashMap<u64, Pending>,
    next_request: u64,
    reads: Option<(Sender<Read>, Receiver<Read>)>,
    jobs: Option<Sender<Job>>,
    /// *Print / PDF* waits for the page to load.
    print: bool,
    blocked_navigations: Vec<String>,
    blocked_requests: Vec<String>,
    served_requests: Vec<String>,
    requests: u32,
    served: u32,
    windows: u32,
    permissions: u32,
    loads: u32,
    app_links: u32,
}

impl State {
    fn slot(&mut self, role: Role) -> &mut Slot {
        &mut self.slots[role as usize]
    }

    /// The role of `web` (one of the views), if it is one.
    fn role_of(&self, web: &WKWebView) -> Option<Role> {
        [Role::Plain, Role::Scripts].into_iter().find(|r| self.slots[*r as usize].view.as_ref().is_some_and(|v| std::ptr::eq(Retained::as_ptr(v).cast::<WKWebView>(), web)))
    }

    /// Queues a file to read through the backend for request `id`'s reply
    /// (`false`: no backend or no reader thread; answer at once).
    fn fetch(&mut self, id: u64, role: Role, path: String, main: bool, scripts: bool) -> bool {
        let Some(backend) = self.backend.clone() else { return false };
        if self.jobs.is_none() {
            let done = self.reads.get_or_insert_with(crossbeam_channel::unbounded).0.clone();
            self.jobs = start_readers(done);
        }
        let live = self.slot(role).generation.clone();
        let generation = live.load(Ordering::Relaxed);
        self.jobs.as_ref().is_some_and(|jobs| jobs.send(Job { id, path, main, scripts, backend, live, generation }).is_ok())
    }
}

fn clip(text: &str) -> String {
    text.chars().take(160).collect()
}

fn push_capped(list: &mut Vec<String>, item: String) {
    list.push(item);
    if list.len() > 12 {
        list.remove(0);
    }
}

fn wake() {
    crate::shell::wake_web();
}

/// The HTML view as a whole cannot work: the core shows pages as formatted text.
fn fail(state: &Rc<RefCell<State>>, why: String) {
    {
        let mut s = state.borrow_mut();
        s.phase = Phase::Failed(why.clone());
        s.events.push(WebEvent::Failed(why));
    }
    wake();
}

/// The scripts view cannot work: notes allowed to run scripts show in the
/// plain view from now on, without them.
fn fail_scripts(state: &Rc<RefCell<State>>, why: String) {
    {
        let mut s = state.borrow_mut();
        s.slot(Role::Scripts).phase = Phase::Failed(why.clone());
        s.events.push(WebEvent::Notice(format!("Scripts in HTML notes are unavailable ({why}); such notes show without them."), true));
    }
    wake();
}

fn error_text(error: *mut NSError) -> String {
    // SAFETY: WebKit hands a valid error or null.
    unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string()).unwrap_or_else(|| "unknown error".into())
}

/// A URL of the notes scheme as the shared rules know it (`https:` origin).
fn shared_url(url: &str) -> String {
    match url.get(..SCHEME_ORIGIN.len()) {
        Some(head) if head.eq_ignore_ascii_case(SCHEME_ORIGIN) => format!("{ORIGIN}{}", &url[SCHEME_ORIGIN.len()..]),
        _ => url.to_owned(),
    }
}

/// A URL of the shared rules' notes origin as this view loads it.
fn scheme_url(url: &str) -> String {
    match url.get(..ORIGIN.len()) {
        Some(head) if head.eq_ignore_ascii_case(ORIGIN) => format!("{SCHEME_ORIGIN}{}", &url[ORIGIN.len()..]),
        _ => url.to_owned(),
    }
}

fn url_text(url: Option<Retained<NSURL>>) -> String {
    // SAFETY: a plain read of a URL WebKit hands over.
    url.and_then(|u| unsafe { u.absoluteString() }).map(|s| s.to_string()).unwrap_or_default()
}

/// Answers a request of the notes scheme (on the main thread, as WebKit's
/// task methods want): the reply's status, headers and body, the origin the
/// policy names being this scheme's.
fn respond(task: &ProtocolObject<dyn WKURLSchemeTask>, url: &NSURL, reply: &Reply) {
    let mut keys: Vec<Retained<NSString>> = Vec::new();
    let mut values: Vec<Retained<NSString>> = Vec::new();
    let mut html = false;
    for line in reply.headers.split("\r\n").filter(|l| !l.is_empty()) {
        if let Some((name, value)) = line.split_once(':') {
            html |= name.eq_ignore_ascii_case("Content-Type") && value.contains("text/html");
            keys.push(NSString::from_str(name.trim()));
            values.push(NSString::from_str(&value.trim().replace(ORIGIN, SCHEME_ORIGIN)));
        }
    }
    let keys: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let headers = NSDictionary::from_vec(&keys, values);
    // The note's page names its origin (the sanitizer's policy, absolute links) as the scheme's.
    let body = if html { String::from_utf8_lossy(&reply.body).replace(ORIGIN, SCHEME_ORIGIN).into_bytes() } else { reply.body.clone() };
    // SAFETY: a live task WebKit has not stopped (stopped ones were taken out
    // of `pending` first), answered on the main thread.
    unsafe {
        let response = NSHTTPURLResponse::initWithURL_statusCode_HTTPVersion_headerFields(NSHTTPURLResponse::alloc(), url, reply.status as isize, Some(&NSString::from_str("HTTP/1.1")), Some(&headers));
        let Some(response) = response else { return };
        task.didReceiveResponse(&response);
        if !body.is_empty() {
            task.didReceiveData(&NSData::with_bytes(&body));
        }
        task.didFinish();
    }
}

/// A view's page is replaced: the requests it still waits on are answered
/// (403) now, and its reads not yet started are skipped.
fn cancel(state: &Rc<RefCell<State>>, role: Role) {
    let stale: Vec<Pending> = {
        let mut s = state.borrow_mut();
        s.slot(role).generation.fetch_add(1, Ordering::Relaxed);
        let ids: Vec<u64> = s.pending.iter().filter(|(_, p)| p.role == role).map(|(id, _)| *id).collect();
        ids.iter().filter_map(|id| s.pending.remove(id)).collect()
    };
    for pending in stale {
        respond(&pending.task, &pending.url, &webpane::refused());
    }
}

/// Starts the threads that read notes' files and make their replies.
fn start_readers(done: Sender<Read>) -> Option<Sender<Job>> {
    let (jobs, queue) = crossbeam_channel::unbounded::<Job>();
    let mut started = 0;
    for n in 0..READERS {
        let (queue, done): (Receiver<Job>, Sender<Read>) = (queue.clone(), done.clone());
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
                if done.send((job.id, reply)).is_err() {
                    break;
                }
                wake();
            }
        });
        started += usize::from(spawned.is_ok());
    }
    (started > 0).then_some(jobs)
}

/// A link the user followed (cancelled in the view): the canvas acts on it.
fn asked(s: &mut State, uri: &str, root: Option<&str>) {
    let event = match webpane::link_action(uri, root) {
        LinkAction::App(space, target) => {
            s.app_links += 1;
            WebEvent::Navigate(space, target)
        }
        LinkAction::Browser(url) => WebEvent::Link(url),
        LinkAction::Ignore => return,
    };
    s.events.push(event);
    wake();
}

/// A key pressed in a page as the canvas's keyboard map has it (Command is
/// Ctrl, ⌘[ is back…), if it is one the page hands over.
fn page_key(event: &NSEvent) -> Option<webpane::WebKey> {
    // SAFETY: plain reads of a key event AppKit delivers.
    let (code, flags, plain) = unsafe { (event.keyCode(), event.modifierFlags(), event.charactersIgnoringModifiers().map(|s| s.to_string()).unwrap_or_default()) };
    let mut mods = Modifiers::NONE;
    for (flag, m) in [
        (NSEventModifierFlags::NSEventModifierFlagCommand, Modifiers::SUPER),
        (NSEventModifierFlags::NSEventModifierFlagControl, Modifiers::CONTROL),
        (NSEventModifierFlags::NSEventModifierFlagOption, Modifiers::ALT),
        (NSEventModifierFlags::NSEventModifierFlagShift, Modifiers::SHIFT),
    ] {
        if flags.contains(flag) {
            mods |= m;
        }
    }
    let key = match code {
        VK_ESCAPE if mods.is_empty() => return Some(webpane::WebKey::Escape),
        VK_F6 => KeyCode::F(6),
        VK_TAB => KeyCode::Tab,
        VK_LEFT => KeyCode::Left,
        VK_RIGHT => KeyCode::Right,
        _ => KeyCode::Char(plain.chars().next()?),
    };
    let key = crate::os::KEY_POLICY.key(KeyEvent::new(key, mods));
    forwarded(&key).then_some(webpane::WebKey::Forward(key))
}

/// The app's keys a page hands over (the Mac's map; `webpane::web_key`'s set).
fn forwarded(key: &KeyEvent) -> bool {
    let m = key.modifiers;
    let app = key.source == KeySource::App;
    let ctrl = m.contains(Modifiers::CONTROL) && !m.contains(Modifiers::ALT);
    match key.code {
        KeyCode::F(6) => true,
        // Control+] (Control is Control on a Mac): leave the page, as a terminal.
        KeyCode::Char(']') if key.source == KeySource::Program && m == Modifiers::CONTROL => true,
        // ⌘[ / ⌘] back and forward, ⌃⌘N a quick note.
        KeyCode::Left | KeyCode::Right if app && m == Modifiers::ALT => true,
        KeyCode::Char('n') if app && m == Modifiers::ALT => true,
        // ⇧⌘[ / ⇧⌘] the tabs.
        KeyCode::Tab | KeyCode::BackTab if app && ctrl => true,
        KeyCode::Char('1' | '2' | '3' | 'p' | 'e' | 'w' | 's' | 'f' | 'n') if app && ctrl => true,
        _ => false,
    }
}

/// The view's ivars: its window's state and which view it is.
struct ViewIvars {
    state: Weak<RefCell<State>>,
    role: Role,
}

declare_class!(
    /// A web view: the app's keys go to the canvas; it tracks the keyboard
    /// focus (app links of the scripts view count once per click into it).
    struct ReportView;

    unsafe impl ClassType for ReportView {
        #[inherits(NSView, NSResponder, NSObject)]
        type Super = WKWebView;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "CopilotCanvasReportView";
    }

    impl DeclaredClass for ReportView {
        type Ivars = ViewIvars;
    }

    unsafe impl ReportView {
        #[method(keyDown:)]
        fn key_down(&self, event: &NSEvent) {
            if !self.hand_over(event) {
                // SAFETY: WKWebView's own handling of other keys.
                let _: () = unsafe { msg_send![super(self), keyDown: event] };
            }
        }

        #[method(performKeyEquivalent:)]
        fn key_equivalent(&self, event: &NSEvent) -> objc2::runtime::Bool {
            // Only while the page has the keyboard (key equivalents go to every view).
            let focused = self.ivars().state.upgrade().is_some_and(|s| s.borrow().slots[self.ivars().role as usize].links.focused());
            if focused && self.hand_over(event) {
                return objc2::runtime::Bool::YES;
            }
            // SAFETY: WKWebView's (and the menus') handling of other key equivalents.
            unsafe { msg_send![super(self), performKeyEquivalent: event] }
        }

        #[method(becomeFirstResponder)]
        fn become_first(&self) -> bool {
            // SAFETY: NSResponder's.
            let became: bool = unsafe { msg_send![super(self), becomeFirstResponder] };
            if became && let Some(state) = self.ivars().state.upgrade() {
                state.borrow_mut().slot(self.ivars().role).links.focus(true);
            }
            became
        }

        #[method(resignFirstResponder)]
        fn resign_first(&self) -> bool {
            // SAFETY: NSResponder's.
            let resigned: bool = unsafe { msg_send![super(self), resignFirstResponder] };
            if resigned && let Some(state) = self.ivars().state.upgrade() {
                state.borrow_mut().slot(self.ivars().role).links.focus(false);
            }
            resigned
        }
    }
);

impl ReportView {
    fn new(state: Weak<RefCell<State>>, role: Role, configuration: &WKWebViewConfiguration, mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(ViewIvars { state, role });
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
        // SAFETY: WKWebView's designated initializer.
        unsafe { msg_send_id![super(this), initWithFrame: frame, configuration: configuration] }
    }

    /// A key the canvas takes from the page: queued (true), else the page's.
    fn hand_over(&self, event: &NSEvent) -> bool {
        let Some(action) = page_key(event) else { return false };
        if let Some(state) = self.ivars().state.upgrade() {
            state.borrow_mut().events.push(match action {
                webpane::WebKey::Escape => WebEvent::Escape,
                webpane::WebKey::Forward(key) => WebEvent::Key(key),
            });
            wake();
        }
        true
    }
}

declare_class!(
    /// Decides navigations, refuses windows and devices, hears the counter.
    struct Delegate;

    unsafe impl ClassType for Delegate {
        type Super = NSObject;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "CopilotCanvasReportDelegate";
    }

    impl DeclaredClass for Delegate {
        type Ivars = Weak<RefCell<State>>;
    }

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl WKNavigationDelegate for Delegate {
        #[method(webView:decidePolicyForNavigationAction:decisionHandler:)]
        fn decide(&self, web: &WKWebView, action: &WKNavigationAction, handler: &block2::Block<dyn Fn(WKNavigationActionPolicy)>) {
            // SAFETY: plain reads of the action and the view WebKit hands over.
            let (uri, kind, main, current) = unsafe {
                let target = action.targetFrame();
                (shared_url(&url_text(action.request().URL())), action.navigationType(), target.map(|f| f.isMainFrame()), shared_url(&url_text(web.URL())))
            };
            let allow = match self.ivars().upgrade() {
                Some(state) => decide(&state, web, &uri, kind, main, &current),
                None => false,
            };
            handler.call((if allow { WKNavigationActionPolicy::Allow } else { WKNavigationActionPolicy::Cancel },));
        }

        #[method(webView:didFinishNavigation:)]
        fn finished(&self, web: &WKWebView, _navigation: Option<&WKNavigation>) {
            let Some(state) = self.ivars().upgrade() else { return };
            let Some(role) = state.borrow().role_of(web) else { return };
            {
                let mut s = state.borrow_mut();
                s.loads += 1;
                s.slot(role).loading = false;
            }
            // What it shows now, for window captures (the window server
            // composes the view; macOS gives no capture of it without asking).
            let weak = Rc::downgrade(&state);
            snapshot(web, move |png| {
                if let (Some(state), Ok(png)) = (weak.upgrade(), png) {
                    state.borrow_mut().slot(role).snapshot = Some(png);
                }
            });
            wake();
        }

        #[method(webViewWebContentProcessDidTerminate:)]
        fn terminated(&self, web: &WKWebView) {
            let Some(state) = self.ivars().upgrade() else { return };
            let role = state.borrow().role_of(web);
            match role {
                Some(Role::Scripts) => fail_scripts(&state, "its web content process ended".into()),
                _ => fail(&state, "the HTML view's web content process ended".into()),
            }
        }
    }

    unsafe impl WKUIDelegate for Delegate {
        /// `target=_blank` and `window.open`: no new window; a clicked link is asked about.
        #[method_id(webView:createWebViewWithConfiguration:forNavigationAction:windowFeatures:)]
        fn create_web_view(&self, web: &WKWebView, _configuration: &WKWebViewConfiguration, action: &WKNavigationAction, _features: &WKWindowFeatures) -> Option<Retained<WKWebView>> {
            // SAFETY: plain reads of the action WebKit hands over.
            let (uri, kind) = unsafe { (shared_url(&url_text(action.request().URL())), action.navigationType()) };
            if let Some(state) = self.ivars().upgrade() {
                let mut s = state.borrow_mut();
                s.windows += 1;
                let root = s.role_of(web).and_then(|r| s.slots[r as usize].serving.as_ref().map(|v| v.root.clone()));
                if kind == WKNavigationType::LinkActivated {
                    asked(&mut s, &uri, root.as_deref());
                }
            }
            None
        }

        /// No camera or microphone for a page.
        #[method(webView:requestMediaCapturePermissionForOrigin:initiatedByFrame:type:decisionHandler:)]
        fn media(&self, _web: &WKWebView, _origin: &WKSecurityOrigin, _frame: &WKFrameInfo, _kind: WKMediaCaptureType, handler: &block2::Block<dyn Fn(WKPermissionDecision)>) {
            if let Some(state) = self.ivars().upgrade() {
                state.borrow_mut().permissions += 1;
            }
            handler.call((WKPermissionDecision::Deny,));
        }
    }

    unsafe impl WKScriptMessageHandler for Delegate {
        /// The counter: a load the rule list refused.
        #[method(userContentController:didReceiveScriptMessage:)]
        fn message(&self, _controller: &WKUserContentController, message: &WKScriptMessage) {
            let Some(state) = self.ivars().upgrade() else { return };
            // SAFETY: the message's body as the counter posts it (a string).
            let body = unsafe { message.body() };
            let uri = json_of(&body).as_str().map(str::to_owned).unwrap_or_default();
            let mut s = state.borrow_mut();
            s.requests += 1;
            push_capped(&mut s.blocked_requests, clip(&uri));
        }
    }
);

impl Delegate {
    fn new(state: Weak<RefCell<State>>, mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(state);
        // SAFETY: NSObject's init.
        unsafe { msg_send_id![super(this), init] }
    }
}

/// Whether a navigation of `web` may go on (the canvas acts on links instead).
fn decide(state: &Rc<RefCell<State>>, web: &WKWebView, uri: &str, kind: WKNavigationType, main: Option<bool>, current: &str) -> bool {
    let mut s = state.borrow_mut();
    let Some(role) = s.role_of(web) else { return false };
    let root = s.slots[role as usize].serving.as_ref().map(|v| v.root.clone());
    // No target frame: a new window (`target=_blank` before `createWebView`).
    if main.is_none() {
        s.windows += 1;
        if kind == WKNavigationType::LinkActivated {
            asked(&mut s, uri, root.as_deref());
        }
        return false;
    }
    let slot = s.slot(role);
    if main == Some(true) && slot.expect.as_ref().is_some_and(|expect| webpane::expected(expect, uri, root.as_deref())) {
        (slot.expect, slot.loading) = (None, true);
        return true;
    }
    // An in-page anchor scrolls; an app link in it is the canvas's.
    if webpane::same_document(current, uri) && webpane::fragment_target(uri).is_none() {
        return true;
    }
    if let Some((space, target)) = webpane::fragment_target(uri) {
        // In the scripts view once per click into it (a script cannot drive the app).
        let visible = slot.visible;
        let clicked = kind == WKNavigationType::LinkActivated;
        if clicked && role == Role::Scripts && !slot.links.focused() {
            slot.links.focus(true);
        }
        if slot.links.follow(role == Role::Scripts, visible, Instant::now()) {
            slot.stale = true;
            s.app_links += 1;
            s.events.push(WebEvent::Navigate(space, target));
            drop(s);
            wake();
        } else {
            push_capped(&mut s.blocked_navigations, clip(uri));
        }
        return false;
    }
    push_capped(&mut s.blocked_navigations, clip(uri));
    if kind == WKNavigationType::LinkActivated {
        asked(&mut s, uri, root.as_deref());
    }
    false
}

declare_class!(
    /// Serves the notes scheme: the files of the note's folder, read through
    /// the backend.
    struct SchemeHandler;

    unsafe impl ClassType for SchemeHandler {
        type Super = NSObject;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "CopilotCanvasNotesScheme";
    }

    impl DeclaredClass for SchemeHandler {
        type Ivars = Weak<RefCell<State>>;
    }

    unsafe impl NSObjectProtocol for SchemeHandler {}

    unsafe impl WKURLSchemeHandler for SchemeHandler {
        #[method(webView:startURLSchemeTask:)]
        fn start(&self, web: &WKWebView, task: &ProtocolObject<dyn WKURLSchemeTask>) {
            // SAFETY: plain reads of the task's request.
            let (url, uri, method, document) = unsafe {
                let request: Retained<NSURLRequest> = task.request();
                let url = request.URL();
                let uri = shared_url(&url_text(url.clone()));
                let method = request.HTTPMethod().map(|m| m.to_string()).unwrap_or_else(|| "GET".into());
                let main_document = shared_url(&url_text(request.mainDocumentURL()));
                (url, uri.clone(), method, main_document == uri)
            };
            let Some(url) = url else { return };
            let Some(state) = self.ivars().upgrade() else {
                return respond(task, &url, &webpane::refused());
            };
            let (role, serving, admitted) = {
                let s = state.borrow();
                let Some(role) = s.role_of(web) else { drop(s); return respond(task, &url, &webpane::refused()) };
                (role, s.slots[role as usize].serving.clone(), webpane::admit(s.pending.len()))
            };
            match webpane::classify(&uri, &method, document, serving.as_ref()) {
                Resource::File { .. } if !admitted => {
                    state.borrow_mut().requests += 1;
                    respond(task, &url, &webpane::busy());
                }
                Resource::File { path, main } => {
                    // The scripts policy and the unsanitized note only ever in the scripts view.
                    let scripts = role == Role::Scripts;
                    // SAFETY: the task WebKit handed over, kept until answered or stopped.
                    let Some(kept) = (unsafe { Retained::retain((task as *const ProtocolObject<dyn WKURLSchemeTask>).cast_mut()) }) else { return };
                    let mut s = state.borrow_mut();
                    s.next_request += 1;
                    let id = s.next_request;
                    if s.fetch(id, role, path.clone(), main, scripts) {
                        s.served += 1;
                        push_capped(&mut s.served_requests, clip(&uri));
                        s.pending.insert(id, Pending { role, task: kept, url });
                    } else {
                        drop(s);
                        respond(task, &url, &webpane::reply(&path, main, scripts, Err("the files cannot be read now".into())));
                    }
                }
                Resource::Refused => {
                    {
                        let mut s = state.borrow_mut();
                        s.requests += 1;
                        push_capped(&mut s.blocked_requests, clip(&uri));
                    }
                    respond(task, &url, &webpane::refused());
                }
            }
        }

        #[method(webView:stopURLSchemeTask:)]
        fn stop(&self, _web: &WKWebView, task: &ProtocolObject<dyn WKURLSchemeTask>) {
            // WebKit gave up on it: never answered from now on.
            let Some(state) = self.ivars().upgrade() else { return };
            let mut s = state.borrow_mut();
            let stopped: Vec<u64> = s.pending.iter().filter(|(_, p)| std::ptr::eq(Retained::as_ptr(&p.task), task)).map(|(id, _)| *id).collect();
            for id in stopped {
                s.pending.remove(&id);
            }
        }
    }
);

impl SchemeHandler {
    fn new(state: Weak<RefCell<State>>, mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(state);
        // SAFETY: NSObject's init.
        unsafe { msg_send_id![super(this), init] }
    }
}

/// A JavaScript or message value as JSON (strings, numbers, booleans, null).
fn json_of(object: &AnyObject) -> Value {
    // SAFETY: class checks before the casts; plain property reads.
    unsafe {
        let is = |class: &objc2::runtime::AnyClass| -> bool { msg_send![object, isKindOfClass: class] };
        if is(NSString::class()) {
            let text = &*(object as *const AnyObject as *const NSString);
            return Value::String(text.to_string());
        }
        if is(NSNumber::class()) {
            let number = &*(object as *const AnyObject as *const NSNumber);
            let kind = std::ffi::CStr::from_ptr(number.objCType().as_ptr()).to_bytes().first().copied();
            return match kind {
                Some(b'c' | b'B') => Value::Bool(number.boolValue()),
                Some(b'f' | b'd') => json!(number.doubleValue()),
                _ => json!(number.longLongValue()),
            };
        }
    }
    let description: Retained<NSString> = unsafe { msg_send_id![object, description] };
    Value::String(description.to_string())
}

/// What a view shows, as PNG bytes, answered through `done` (on the main thread).
fn snapshot(web: &WKWebView, done: impl Fn(Result<Vec<u8>, String>) + 'static) {
    let block = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
        // SAFETY: WebKit hands a valid image or null.
        let png = match unsafe { image.as_ref() } {
            Some(image) => png_of(image).ok_or_else(|| "the snapshot could not be encoded".to_owned()),
            None => Err(error_text(error)),
        };
        done(png);
    });
    // SAFETY: a live view of this (main) thread; the block is reference counted.
    unsafe { web.takeSnapshotWithConfiguration_completionHandler(None, &block) };
}

fn png_of(image: &NSImage) -> Option<Vec<u8>> {
    // SAFETY: AppKit's image conversions on an image WebKit made.
    unsafe {
        let tiff = image.TIFFRepresentation()?;
        let rep = NSBitmapImageRep::imageRepWithData(&tiff)?;
        let png = rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())?;
        Some(png.bytes().to_vec())
    }
}

/// Where *Print / PDF* puts the PDF: `COPILOT_CANVAS_DOWNLOADS` (tests), else
/// `~/Downloads`, else the temporary folder.
fn downloads() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("COPILOT_CANVAS_DOWNLOADS").filter(|d| !d.is_empty()) {
        return std::path::PathBuf::from(dir);
    }
    std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join("Downloads")).filter(|dir| dir.is_dir()).unwrap_or_else(std::env::temp_dir)
}

/// What a view loads for a pane.
enum Page {
    /// A whole page as a string.
    Document(Arc<String>),
    /// An HTML note at its URL of the notes scheme.
    File { url: String, serving: Serving },
}

/// One window's HTML view.
pub struct ReportWeb {
    state: Rc<RefCell<State>>,
    /// Where the compiled rule list is kept.
    rules: std::path::PathBuf,
    /// The canvas window's content view.
    parent: Cell<isize>,
}

impl ReportWeb {
    pub fn new(home: &std::path::Path) -> Self {
        ReportWeb { state: Rc::default(), rules: home.join("webkit-rules"), parent: Cell::new(0) }
    }

    /// What happened since the last call.
    pub fn take_events(&mut self) -> Vec<WebEvent> {
        std::mem::take(&mut self.state.borrow_mut().events)
    }

    /// The view's latest frame, for views the GUI draws itself (none here:
    /// these views are drawn by the window server).
    pub fn frame(&self) -> Option<crate::scene::ImageItem> {
        None
    }

    /// Input for views the GUI draws itself; these get their own from AppKit.
    pub fn input(&mut self, _event: &crate::os::WebInput) -> bool {
        false
    }

    fn view(&self, role: Role) -> Option<Retained<ReportView>> {
        self.state.borrow().slots[role as usize].view.clone()
    }

    /// Shows `pane` over its area, or hides the views (`None`). A whole page
    /// (reports, rendered Markdown) loads as a string in the plain view; an
    /// HTML note from the notes scheme, in the scripts view when its scripts
    /// may run (in the plain one, without them, if that view could not
    /// start). The views are created when first needed; `backend` reads the
    /// notes' files.
    pub fn sync(&mut self, parent: isize, pane: Option<&HtmlPane>, backend: &Arc<dyn Backend>, cell_w: f32, cell_h: f32) {
        self.parent.set(parent);
        self.state.borrow_mut().backend = Some(backend.clone());
        let Some(pane) = pane else {
            self.state.borrow_mut().print = false;
            self.hide(Role::Plain);
            self.hide(Role::Scripts);
            return;
        };
        let (role, page) = self.page_of(pane);
        self.hide(role.other());
        let phase = self.state.borrow().phase.clone();
        match phase {
            Phase::Idle => return self.create(),
            Phase::Creating | Phase::Failed(_) => return,
            Phase::Ready => {}
        }
        let view_phase = self.state.borrow().slots[role as usize].phase.clone();
        match view_phase {
            // The plain view comes with the rule list; the scripts view when first needed.
            Phase::Idle => {
                if let Err(why) = build(&self.state, role, parent) {
                    fail_scripts(&self.state, why);
                }
            }
            Phase::Creating | Phase::Failed(_) => {}
            Phase::Ready => self.show(role, pane, page, cell_w, cell_h),
        }
    }

    /// The view a pane shows in and the page it loads there.
    fn page_of(&self, pane: &HtmlPane) -> (Role, Page) {
        match &pane.source {
            HtmlSource::Document(document) => (Role::Plain, Page::Document(document.clone())),
            HtmlSource::File { path, root, scripts } => {
                let scripts_failed = matches!(self.state.borrow().slots[Role::Scripts as usize].phase, Phase::Failed(_));
                let role = if *scripts && !scripts_failed { Role::Scripts } else { Role::Plain };
                match webpane::url_for(root, path) {
                    Some(url) => (role, Page::File { url: scheme_url(&url), serving: Serving { root: root.clone(), main: path.clone(), scripts: role == Role::Scripts } }),
                    None => {
                        let message = format!("{path} cannot be shown here: it is not a file inside {root}.");
                        (Role::Plain, Page::Document(Arc::new(canvas_core::report_html::notes::message_page(&message))))
                    }
                }
            }
        }
    }

    /// Places a ready view over the pane, loads its page when it changed
    /// (or still names an app link it followed) and shows it.
    fn show(&mut self, role: Role, pane: &HtmlPane, page: Page, cell_w: f32, cell_h: f32) {
        let Some(view) = self.view(role) else { return };
        let scale = view.window().map(|w| w.backingScaleFactor()).unwrap_or(1.0).max(0.5);
        let r = pane.rect;
        let bounds = [
            (r.x as f32 * cell_w).round() as i32,
            (r.y as f32 * cell_h).round() as i32,
            ((r.x + r.width) as f32 * cell_w).round() as i32,
            ((r.y + r.height) as f32 * cell_h).round() as i32,
        ];
        // The content view is flipped (winit): the origin is its top left.
        let frame = [bounds[0] as f64 / scale, bounds[1] as f64 / scale, (bounds[2] - bounds[0]) as f64 / scale, (bounds[3] - bounds[1]) as f64 / scale];
        let moved = self.state.borrow_mut().slot(role).frame.replace(frame) != Some(frame);
        if moved {
            self.state.borrow_mut().slot(role).bounds = Some(bounds);
            // SAFETY: a live view of this thread.
            unsafe { view.setFrame(NSRect::new(NSPoint::new(frame[0], frame[1]), NSSize::new(frame[2], frame[3]))) };
        }
        let serving = match &page {
            Page::Document(_) => None,
            Page::File { serving, .. } => Some(serving.clone()),
        };
        let shown: Shown = (pane.id.clone(), serving.clone());
        let reload = {
            let s = self.state.borrow();
            let slot = &s.slots[role as usize];
            slot.shown.as_ref() != Some(&shown) || slot.stale
        };
        if reload {
            cancel(&self.state, role);
            {
                let mut s = self.state.borrow_mut();
                let slot = s.slot(role);
                // The view serves the new note (or nothing) before its load starts.
                slot.expect = Some(match &serving {
                    Some(serving) => Expect::File(serving.main.clone()),
                    None => Expect::Page,
                });
                (slot.serving, slot.shown, slot.stale, slot.loading, slot.snapshot) = (serving, Some(shown), false, true, None);
            }
            // SAFETY: a live view of this thread; WebKit copies what it is given.
            unsafe {
                match &page {
                    Page::Document(document) => {
                        view.loadHTMLString_baseURL(&NSString::from_str(document), None);
                    }
                    Page::File { url, .. } => {
                        let Some(url) = NSURL::URLWithString(&NSString::from_str(url)) else { return };
                        view.loadRequest(&NSURLRequest::requestWithURL(&url));
                    }
                }
            }
        }
        if !self.state.borrow().slots[role as usize].visible {
            view.setHidden(false);
            self.state.borrow_mut().slot(role).visible = true;
        }
    }

    /// Hides a view. The scripts view also unloads its page: no note's
    /// script runs out of sight. The keyboard goes back to the canvas.
    fn hide(&mut self, role: Role) {
        let Some(view) = self.view(role) else { return };
        if !self.state.borrow().slots[role as usize].visible {
            return;
        }
        self.blur();
        view.setHidden(true);
        {
            let mut s = self.state.borrow_mut();
            let slot = s.slot(role);
            slot.visible = false;
            if role == Role::Scripts {
                // Only its own about:blank may load now (its scripts still run until then).
                (slot.shown, slot.serving, slot.loading, slot.expect) = (None, None, false, Some(Expect::Blank));
            }
        }
        if role == Role::Scripts {
            cancel(&self.state, role);
            // SAFETY: a live view of this thread.
            unsafe {
                if let Some(blank) = NSURL::URLWithString(&NSString::from_str("about:blank")) {
                    view.loadRequest(&NSURLRequest::requestWithURL(&blank));
                }
            }
        }
    }

    /// Gives the keyboard back to the canvas if a view has it.
    pub fn blur(&self) {
        let parent = self.parent.get();
        for role in [Role::Plain, Role::Scripts] {
            let Some(view) = self.view(role) else { continue };
            let Some(window) = view.window() else { continue };
            let focused = window.firstResponder();
            // SAFETY: plain AppKit queries on this thread's views.
            let inside = focused.is_some_and(|r| unsafe {
                let is_view: bool = msg_send![&*r, isKindOfClass: NSView::class()];
                is_view && msg_send![&*(Retained::as_ptr(&r) as *const NSView), isDescendantOf: &**view]
            });
            if inside && parent != 0 {
                // SAFETY: the canvas window's live content view.
                let parent = unsafe { &*(parent as *const NSResponder) };
                window.makeFirstResponder(Some(parent));
            }
            self.state.borrow_mut().slot(role).links.focus(false);
        }
    }

    /// Compiles the rule list, then creates the plain view (never without it).
    fn create(&mut self) {
        let parent = self.parent.get();
        if parent == 0 {
            return;
        }
        self.state.borrow_mut().phase = Phase::Creating;
        if let Err(error) = std::fs::create_dir_all(&self.rules) {
            return fail(&self.state, format!("the HTML view cannot keep its rules: {error}"));
        }
        // SAFETY: WebKit's rule store in a folder of the canvas's.
        let store = unsafe { WKContentRuleListStore::storeWithURL(Some(&NSURL::fileURLWithPath(&NSString::from_str(&self.rules.display().to_string())))) };
        let Some(store) = store else { return fail(&self.state, "the HTML view cannot block network loads (no rule store)".into()) };
        let weak = Rc::downgrade(&self.state);
        let block = RcBlock::new(move |list: *mut WKContentRuleList, error: *mut NSError| {
            let Some(state) = weak.upgrade() else { return };
            // SAFETY: WebKit hands a valid list or null.
            let Some(list) = (unsafe { Retained::retain(list) }) else {
                return fail(&state, format!("the HTML view cannot block network loads: {}", error_text(error)));
            };
            state.borrow_mut().rules = Some(list);
            match build(&state, Role::Plain, parent) {
                Ok(()) => {
                    {
                        let mut s = state.borrow_mut();
                        s.phase = Phase::Ready;
                        s.events.push(WebEvent::Ready);
                    }
                    wake();
                }
                Err(why) => fail(&state, why),
            }
        });
        // SAFETY: the strings are copied; the block is reference counted.
        unsafe { store.compileContentRuleListForIdentifier_encodedContentRuleList_completionHandler(Some(&NSString::from_str(RULES_ID)), Some(&NSString::from_str(RULES)), Some(&block)) };
    }

    /// Answers the requests whose files were read (on the main thread, as
    /// WebKit wants) and prints a page that waited to load.
    pub fn pump(&mut self) {
        let reads: Vec<Read> = self.state.borrow().reads.as_ref().map(|(_, rx)| rx.try_iter().collect()).unwrap_or_default();
        for (id, reply) in reads {
            let pending = self.state.borrow_mut().pending.remove(&id);
            if let Some(pending) = pending {
                respond(&pending.task, &pending.url, &reply);
            }
        }
        if self.state.borrow().print {
            self.run_print();
        }
    }

    /// *Print / PDF*: the shown page as a PDF in Downloads, once it has loaded.
    pub fn print(&mut self) {
        self.state.borrow_mut().print = true;
        self.run_print();
    }

    fn run_print(&mut self) {
        let (view, failed) = {
            let s = self.state.borrow();
            let shown = [Role::Plain, Role::Scripts].into_iter().find(|r| s.slots[*r as usize].visible);
            match (&s.phase, shown) {
                (Phase::Failed(why), _) => (None, Some(format!("Printing needs the HTML view: {why}."))),
                (_, Some(role)) if !s.slots[role as usize].loading => (s.slots[role as usize].view.clone(), None),
                // Not shown or still loading: it waits (a pane that goes away cancels it).
                _ => return,
            }
        };
        self.state.borrow_mut().print = false;
        if let Some(why) = failed {
            return self.notice(why, true);
        }
        let Some(view) = view else { return };
        // SAFETY: a plain read of a live view.
        let title = unsafe { view.title() }.map(|t| t.to_string()).unwrap_or_default();
        let path = match webpane::pdf_path(&downloads(), &title, webpane::claim_new) {
            Ok(path) => path,
            Err(why) => return self.notice(format!("The PDF could not be saved: {why}"), true),
        };
        let (weak, claimed) = (Rc::downgrade(&self.state), path.clone());
        let block = RcBlock::new(move |data: *mut NSData, error: *mut NSError| {
            // SAFETY: WebKit hands valid data or null.
            let written = match unsafe { data.as_ref() } {
                Some(data) => std::fs::write(&claimed, data.bytes()).map_err(|e| e.to_string()),
                None => Err(error_text(error)),
            };
            let event = match written {
                Ok(()) => WebEvent::Notice(format!("Saved as PDF: {}", claimed.display()), false),
                Err(why) => {
                    // Our claim, still empty: no empty file stays behind.
                    if std::fs::metadata(&claimed).is_ok_and(|m| m.len() == 0) {
                        let _ = std::fs::remove_file(&claimed);
                    }
                    WebEvent::Notice(format!("The PDF could not be saved: {why}"), true)
                }
            };
            if let Some(state) = weak.upgrade() {
                state.borrow_mut().events.push(event);
                wake();
            }
        });
        // SAFETY: a live view of this thread; the block is reference counted.
        unsafe { view.createPDFWithConfiguration_completionHandler(None, &block) };
    }

    fn notice(&mut self, text: String, error: bool) {
        self.state.borrow_mut().events.push(WebEvent::Notice(text, error));
        wake();
    }

    /// The view shown (else the plain one): the test hooks look at it.
    fn active(&self) -> Role {
        if self.state.borrow().slots[Role::Scripts as usize].visible { Role::Scripts } else { Role::Plain }
    }

    /// The views' state for the test hooks.
    pub fn json(&self) -> Value {
        let role = self.active();
        let s = self.state.borrow();
        let slot = &s.slots[role as usize];
        // SAFETY: plain property reads on this thread's view.
        let (title, source) = slot.view.as_ref().map_or((None, None), |v| unsafe { (v.title().map(|t| t.to_string()), Some(clip(&shared_url(&url_text(v.URL()))))) });
        let views: Vec<Value> = [Role::Plain, Role::Scripts]
            .iter()
            .map(|r| {
                let v = &s.slots[*r as usize];
                let serving = v.serving.as_ref().map(|x| json!({ "root": x.root, "main": x.main, "scripts": x.scripts }));
                json!({ "view": r.name(), "phase": phase_text(&v.phase), "visible": v.visible, "shown": v.shown.as_ref().map(|(id, _)| id), "serving": serving, "loading": v.loading, "focused": v.links.focused() })
            })
            .collect();
        json!({
            "ok": true,
            "engine": "WKWebView",
            "phase": phase_text(&s.phase),
            "visible": slot.visible,
            "bounds": slot.bounds,
            "report": slot.shown.as_ref().map(|(id, _)| id),
            "view": role.name(),
            "views": views,
            "title": title,
            "source": source,
            "loads": s.loads,
            "blocked_navigations": s.blocked_navigations,
            "blocked_requests": s.requests,
            "blocked_request_uris": s.blocked_requests,
            "served_requests": s.served,
            "served_request_uris": s.served_requests,
            "pending_requests": s.pending.len(),
            "app_links": s.app_links,
            "print_pending": s.print,
            "blocked_windows": s.windows,
            "blocked_downloads": 0,
            "denied_permissions": s.permissions,
            "folder": self.rules,
        })
    }

    /// Runs script in the shown page as the host, in the canvas's own world
    /// (test hooks only: the plain view's pages never run their own), and
    /// answers its result through `reply`.
    pub fn eval(&self, script: &str, reply: crossbeam_channel::Sender<Value>) {
        let Some(view) = self.view(self.active()) else {
            let _ = reply.send(json!({ "ok": false, "error": "no HTML view" }));
            return;
        };
        let block = RcBlock::new(move |result: *mut AnyObject, error: *mut NSError| {
            // SAFETY: WebKit hands a valid result (or null) and error (or null).
            let answer = if error.is_null() {
                json!({ "ok": true, "result": unsafe { result.as_ref() }.map_or(Value::Null, json_of) })
            } else {
                json!({ "ok": false, "error": error_text(error) })
            };
            let _ = reply.send(answer);
        });
        // SAFETY: a live view of this thread; the block is reference counted.
        unsafe { view.evaluateJavaScript_inFrame_inContentWorld_completionHandler(&NSString::from_str(script), None, &WKContentWorld::defaultClientWorld(), Some(&block)) };
    }

    /// Loads HTML as it is in the plain view, without the canvas's sanitizer
    /// (test hooks only: checks that the view's own lock-down holds by itself).
    pub fn probe_raw(&mut self, html: &str) -> Value {
        let Some(view) = self.view(Role::Plain) else { return json!({ "ok": false, "error": "no HTML view" }) };
        cancel(&self.state, Role::Plain);
        {
            let mut s = self.state.borrow_mut();
            let slot = s.slot(Role::Plain);
            (slot.expect, slot.serving, slot.loading) = (Some(Expect::Page), None, true);
        }
        // SAFETY: a live view of this thread; WebKit copies the string.
        unsafe { view.loadHTMLString_baseURL(&NSString::from_str(html), None) };
        json!({ "ok": true })
    }

    /// Captures what the shown view shows as a PNG file (test hooks), answered through `reply`.
    pub fn capture(&self, path: std::path::PathBuf, reply: crossbeam_channel::Sender<Value>) {
        let Some(view) = self.view(self.active()) else {
            let _ = reply.send(json!({ "ok": false, "error": "no HTML view" }));
            return;
        };
        snapshot(&view, move |png| {
            let answer = match png.and_then(|bytes| std::fs::write(&path, &bytes).map(|()| bytes.len()).map_err(|e| e.to_string())) {
                Ok(bytes) => json!({ "ok": true, "path": path, "bytes": bytes }),
                Err(error) => json!({ "ok": false, "error": error }),
            };
            let _ = reply.send(answer);
        });
    }

    /// Draws what the shown view showed after its latest load onto `png`
    /// (the window's own frame) where the view is: the window as the window
    /// server composes it, for the test hooks.
    pub fn overlay(&self, png: &std::path::Path) -> anyhow::Result<()> {
        let role = self.active();
        let (view, visible, snapshot) = {
            let s = self.state.borrow();
            let slot = &s.slots[role as usize];
            (slot.view.clone(), slot.visible, slot.snapshot.clone())
        };
        let (Some(view), true, Some(snapshot)) = (view, visible, snapshot) else { return Ok(()) };
        let (sw, sh, shot) = crate::images::decode_png(&snapshot).ok_or_else(|| anyhow::anyhow!("the view's snapshot does not decode"))?;
        let (ww, wh, mut window) = crate::images::decode_png(&std::fs::read(png)?).ok_or_else(|| anyhow::anyhow!("the window capture does not decode"))?;
        let scale = view.window().map(|w| w.backingScaleFactor()).unwrap_or(1.0);
        let f = view.frame();
        let (x0, y0) = ((f.origin.x * scale).round() as i64, (f.origin.y * scale).round() as i64);
        let (w, h) = ((f.size.width * scale).round().max(1.0) as i64, (f.size.height * scale).round().max(1.0) as i64);
        for y in 0..h {
            for x in 0..w {
                let (wx, wy) = (x0 + x, y0 + y);
                if wx < 0 || wy < 0 || wx >= ww as i64 || wy >= wh as i64 {
                    continue;
                }
                let (sx, sy) = ((x * sw as i64 / w) as usize, (y * sh as i64 / h) as usize);
                let from = (sy * sw as usize + sx) * 4;
                let to = (wy as usize * ww as usize + wx as usize) * 4;
                window[to..to + 4].copy_from_slice(&shot[from..from + 4]);
            }
        }
        crate::gpu::write_png(png, ww, wh, &window)
    }
}

/// A view's locked-down configuration and the view, hidden in the canvas
/// window: the plain one with the page's JavaScript off for good, the
/// scripts one with it on and a data store of its own; both under the rule
/// list, serving the notes scheme.
fn build(state: &Rc<RefCell<State>>, role: Role, parent: isize) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("the HTML view needs the main thread")?;
    let rules = state.borrow().rules.clone().ok_or("the HTML view has no rule list")?;
    let weak = Rc::downgrade(state);
    state.borrow_mut().slot(role).phase = Phase::Creating;
    let delegate = {
        let existing = state.borrow().delegate.clone();
        existing.unwrap_or_else(|| Delegate::new(weak.clone(), mtm))
    };
    let scheme = {
        let existing = state.borrow().scheme.clone();
        existing.unwrap_or_else(|| SchemeHandler::new(weak.clone(), mtm))
    };
    // SAFETY: WebKit configuration calls on the main thread before the view exists.
    let view = unsafe {
        let configuration = WKWebViewConfiguration::new();
        // Each call makes a new store: the views share nothing, nothing is kept.
        configuration.setWebsiteDataStore(&WKWebsiteDataStore::nonPersistentDataStore());
        configuration.defaultWebpagePreferences().setAllowsContentJavaScript(role == Role::Scripts);
        let preferences = configuration.preferences();
        preferences.setJavaScriptCanOpenWindowsAutomatically(false);
        preferences.setFraudulentWebsiteWarningEnabled(false);
        configuration.setMediaTypesRequiringUserActionForPlayback(WKAudiovisualMediaTypes::WKAudiovisualMediaTypeAll);
        configuration.setURLSchemeHandler_forURLScheme(Some(ProtocolObject::from_ref(&*scheme)), &NSString::from_str(SCHEME));
        let content = configuration.userContentController();
        content.addContentRuleList(&rules);
        let world = WKContentWorld::worldWithName(&NSString::from_str(WORLD));
        let counter = WKUserScript::initWithSource_injectionTime_forMainFrameOnly_inContentWorld(WKUserScript::alloc(), &NSString::from_str(COUNTER), WKUserScriptInjectionTime::AtDocumentStart, false, &world);
        content.addUserScript(&counter);
        content.addScriptMessageHandler_contentWorld_name(ProtocolObject::from_ref(&*delegate), &world, &NSString::from_str("blocked"));
        let view = ReportView::new(weak, role, &configuration, mtm);
        view.setNavigationDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        view.setUIDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        view.setAllowsLinkPreview(false);
        view.setAllowsMagnification(false);
        view.setAllowsBackForwardNavigationGestures(false);
        if view.respondsToSelector(sel!(setInspectable:)) {
            view.setInspectable(false);
        }
        if view.respondsToSelector(sel!(setUnderPageBackgroundColor:)) {
            view.setUnderPageBackgroundColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(BACKGROUND[0], BACKGROUND[1], BACKGROUND[2], 1.0)));
        }
        view.setHidden(true);
        let parent = &*(parent as *const NSView);
        parent.addSubview(&view);
        view
    };
    let mut s = state.borrow_mut();
    (s.delegate, s.scheme) = (Some(delegate), Some(scheme));
    let slot = s.slot(role);
    (slot.view, slot.phase) = (Some(view), Phase::Ready);
    Ok(())
}

impl Drop for ReportWeb {
    fn drop(&mut self) {
        // No request is left waiting; reads not yet started are skipped.
        for role in [Role::Plain, Role::Scripts] {
            cancel(&self.state, role);
        }
        let mut s = self.state.borrow_mut();
        for slot in s.slots.iter_mut() {
            if let Some(view) = slot.view.take() {
                // SAFETY: this thread's view, taken out of the window it is in.
                unsafe {
                    view.configuration().userContentController().removeAllScriptMessageHandlers();
                    view.setNavigationDelegate(None);
                    view.setUIDelegate(None);
                    view.removeFromSuperview();
                }
            }
        }
        (s.delegate, s.scheme) = (None, None);
    }
}
