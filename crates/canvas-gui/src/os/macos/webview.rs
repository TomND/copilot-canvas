//! The GUI's HTML view of reports on macOS (MAC-LINUX-PLAN D9, §5.7): a
//! WKWebView, a child view of the canvas window's content view over the
//! report viewer's content area. Agent-authored HTML is data, never code; on
//! top of the core's sanitizer and the page's Content-Security-Policy the
//! view is locked down as the iPhone app's (`ReportWebView.swift`):
//!
//! - the page's JavaScript is off (`allowsContentJavaScript`); no new windows,
//!   link previews, magnification, back/forward gestures or Web Inspector; no
//!   fraudulent-website lookups (they would ask a server); media only on a
//!   user action;
//! - a content rule list (compiled into `<home>/webkit-rules`) blocks every
//!   load with an address: `http(s)`, `ws(s)`, `ftp`, `file`, `blob`; the page
//!   arrives as a string, pictures only as `data:` URIs. The view is never
//!   created without it;
//! - every navigation but the canvas's own load of the page (and jumps within
//!   it) is cancelled; a link the user clicks becomes a request to the canvas,
//!   which asks before opening it in the browser;
//! - camera and microphone requests denied; a non-persistent data store.
//!
//! A script of the canvas's own, in an isolated world the page cannot see,
//! counts the loads the rule list refused (it listens for their failures;
//! the page's scripts never run). The view takes the keyboard only after a
//! click into it; Esc there closes the report.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use block2::RcBlock;
use canvas_core::view::HtmlPane;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{ClassType, DeclaredClass, declare_class, msg_send, msg_send_id, mutability, sel};
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSColor, NSEvent, NSImage, NSResponder, NSView};
use objc2_foundation::{MainThreadMarker, NSDictionary, NSError, NSNumber, NSPoint, NSRect, NSSize, NSString, NSURL};
use objc2_web_kit::{
    WKAudiovisualMediaTypes, WKContentRuleList, WKContentRuleListStore, WKContentWorld, WKFrameInfo, WKMediaCaptureType, WKNavigation, WKNavigationAction, WKNavigationActionPolicy,
    WKNavigationDelegate, WKNavigationType, WKPermissionDecision, WKScriptMessage, WKScriptMessageHandler, WKSecurityOrigin, WKUIDelegate, WKUserContentController, WKUserScript,
    WKUserScriptInjectionTime, WKWebView, WKWebViewConfiguration, WKWebsiteDataStore, WKWindowFeatures,
};
use serde_json::{Value, json};

/// Loads with an address are blocked; `data:` and the page's own
/// `about:blank` are not (the iPhone app's list).
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
    if (u && !String(u).startsWith('data:')) {
      try { window.webkit.messageHandlers.blocked.postMessage(String(u)); } catch (_) {}
    }
  }, true);
})();"#;
const WORLD: &str = "CopilotCanvasLockdown";
/// The window's background under the page (the canvas's).
const BACKGROUND: [f64; 3] = [13.0 / 255.0, 17.0 / 255.0, 23.0 / 255.0];
/// `kVK_Escape`.
const ESCAPE: u16 = 53;

/// What happened in the view that the window acts on.
pub enum WebEvent {
    Ready,
    Failed(String),
    /// The user clicked a link (the canvas asks before opening it).
    Link(String),
    /// Esc in the view: the report closes.
    Escape,
    /// A global key pressed in the page, for the canvas.
    #[allow(dead_code)]
    Key(canvas_core::input::KeyEvent),
    /// A `#note:` / `#todo:` link or a link to another file of the note's folder.
    #[allow(dead_code)]
    Navigate(canvas_core::spaces::Space, canvas_core::spaces::NavTarget),
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

#[derive(Default)]
struct State {
    phase: Phase,
    view: Option<Retained<ReportView>>,
    delegate: Option<Retained<Delegate>>,
    /// The canvas's own next navigation (the report's page) may start.
    expect: bool,
    events: Vec<WebEvent>,
    /// What was refused (newest last, a few kept) and how often.
    blocked_navigations: Vec<String>,
    blocked_requests: Vec<String>,
    requests: u32,
    windows: u32,
    permissions: u32,
    loads: u32,
    /// What the view showed after its latest load (PNG), for window captures.
    snapshot: Option<Vec<u8>>,
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

fn fail(state: &Rc<RefCell<State>>, why: String) {
    {
        let mut s = state.borrow_mut();
        s.phase = Phase::Failed(why.clone());
        s.events.push(WebEvent::Failed(why));
    }
    wake();
}

fn error_text(error: *mut NSError) -> String {
    // SAFETY: WebKit hands a valid error or null.
    unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string()).unwrap_or_else(|| "unknown error".into())
}

declare_class!(
    /// The web view: Esc closes the report instead of reaching the page.
    struct ReportView;

    unsafe impl ClassType for ReportView {
        #[inherits(NSView, NSResponder, NSObject)]
        type Super = WKWebView;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "CopilotCanvasReportView";
    }

    impl DeclaredClass for ReportView {
        type Ivars = Weak<RefCell<State>>;
    }

    unsafe impl ReportView {
        #[method(keyDown:)]
        fn key_down(&self, event: &NSEvent) {
            // SAFETY: a key event AppKit delivers.
            if unsafe { event.keyCode() } == ESCAPE {
                if let Some(state) = self.ivars().upgrade() {
                    state.borrow_mut().events.push(WebEvent::Escape);
                    wake();
                }
                return;
            }
            // SAFETY: WKWebView's own handling of other keys.
            let _: () = unsafe { msg_send![super(self), keyDown: event] };
        }
    }
);

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
        fn decide(&self, _web: &WKWebView, action: &WKNavigationAction, handler: &block2::Block<dyn Fn(WKNavigationActionPolicy)>) {
            // SAFETY: plain reads of the action WebKit hands over.
            let (url, kind, main) = unsafe {
                let url = action.request().URL().and_then(|u| u.absoluteString()).map(|s| s.to_string()).unwrap_or_default();
                (url, action.navigationType(), action.targetFrame().is_some_and(|f| f.isMainFrame()))
            };
            let Some(state) = self.ivars().upgrade() else {
                handler.call((WKNavigationActionPolicy::Cancel,));
                return;
            };
            let mut s = state.borrow_mut();
            // The page itself (`loadHTMLString` without a base: about:blank), once.
            if s.expect && kind == WKNavigationType::Other && url == "about:blank" && main {
                s.expect = false;
                drop(s);
                handler.call((WKNavigationActionPolicy::Allow,));
                return;
            }
            // A jump within the page (a table of contents).
            if url.starts_with("about:blank#") && main {
                drop(s);
                handler.call((WKNavigationActionPolicy::Allow,));
                return;
            }
            push_capped(&mut s.blocked_navigations, clip(&url));
            let link = kind == WKNavigationType::LinkActivated && canvas_core::reports_model::check_url(&url).is_ok();
            if link {
                s.events.push(WebEvent::Link(url));
            }
            drop(s);
            handler.call((WKNavigationActionPolicy::Cancel,));
            if link {
                wake();
            }
        }

        #[method(webView:didFinishNavigation:)]
        fn finished(&self, web: &WKWebView, _navigation: Option<&WKNavigation>) {
            let Some(state) = self.ivars().upgrade() else { return };
            state.borrow_mut().loads += 1;
            // What it shows now, for window captures (the window server
            // composes the view; macOS gives no capture of it without asking).
            let weak = Rc::downgrade(&state);
            snapshot(web, move |png| {
                if let (Some(state), Ok(png)) = (weak.upgrade(), png) {
                    state.borrow_mut().snapshot = Some(png);
                }
            });
            wake();
        }

        #[method(webViewWebContentProcessDidTerminate:)]
        fn terminated(&self, _web: &WKWebView) {
            if let Some(state) = self.ivars().upgrade() {
                fail(&state, "the HTML view's web content process ended".into());
            }
        }
    }

    unsafe impl WKUIDelegate for Delegate {
        /// `target=_blank` and `window.open`: no new window; a clicked link is asked about.
        #[method_id(webView:createWebViewWithConfiguration:forNavigationAction:windowFeatures:)]
        fn create_web_view(&self, _web: &WKWebView, _configuration: &WKWebViewConfiguration, action: &WKNavigationAction, _features: &WKWindowFeatures) -> Option<Retained<WKWebView>> {
            // SAFETY: plain reads of the action WebKit hands over.
            let (url, kind) = unsafe { (action.request().URL().and_then(|u| u.absoluteString()).map(|s| s.to_string()).unwrap_or_default(), action.navigationType()) };
            if let Some(state) = self.ivars().upgrade() {
                let mut s = state.borrow_mut();
                s.windows += 1;
                if kind == WKNavigationType::LinkActivated && canvas_core::reports_model::check_url(&url).is_ok() {
                    s.events.push(WebEvent::Link(url));
                    drop(s);
                    wake();
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

impl ReportView {
    fn new(state: Weak<RefCell<State>>, configuration: &WKWebViewConfiguration, mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(state);
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
        // SAFETY: WKWebView's designated initializer.
        unsafe { msg_send_id![super(this), initWithFrame: frame, configuration: configuration] }
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

/// What the view shows, as PNG bytes, answered through `done` (on the main thread).
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

/// One window's HTML view.
pub struct ReportWeb {
    state: Rc<RefCell<State>>,
    /// Where the compiled rule list is kept.
    rules: std::path::PathBuf,
    /// The report whose page is loaded.
    shown: Option<String>,
    visible: bool,
    /// The view's frame in the window (points) and in pixels.
    frame: Option<[f64; 4]>,
    bounds: Option<[i32; 4]>,
    /// The canvas window's content view.
    parent: isize,
}

impl ReportWeb {
    pub fn new(home: &std::path::Path) -> Self {
        ReportWeb { state: Rc::default(), rules: home.join("webkit-rules"), shown: None, visible: false, frame: None, bounds: None, parent: 0 }
    }

    /// What happened since the last call.
    pub fn take_events(&mut self) -> Vec<WebEvent> {
        std::mem::take(&mut self.state.borrow_mut().events)
    }

    /// The view's latest frame, for views the GUI draws itself (none here:
    /// this view is drawn by the window server).
    pub fn frame(&self) -> Option<crate::scene::ImageItem> {
        None
    }

    /// Input for views the GUI draws itself; this one gets its own from AppKit.
    pub fn input(&mut self, _event: &crate::os::WebInput) -> bool {
        false
    }

    fn view(&self) -> Option<Retained<ReportView>> {
        self.state.borrow().view.clone()
    }

    /// Shows `pane` (an HTML report) over its area, or hides the view. The
    /// view is created the first time one is shown.
    pub fn sync(&mut self, parent: isize, pane: Option<&HtmlPane>, _backend: &std::sync::Arc<dyn canvas_core::rpc::Backend>, cell_w: f32, cell_h: f32) {
        self.parent = parent;
        let Some(pane) = pane else { return self.hide() };
        let phase = self.state.borrow().phase.clone();
        match phase {
            Phase::Idle => self.create(),
            Phase::Creating | Phase::Failed(_) => {}
            Phase::Ready => {
                let Some(view) = self.view() else { return };
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
                if self.frame != Some(frame) {
                    // SAFETY: a live view of this thread.
                    unsafe { view.setFrame(NSRect::new(NSPoint::new(frame[0], frame[1]), NSSize::new(frame[2], frame[3]))) };
                    self.frame = Some(frame);
                    self.bounds = Some(bounds);
                }
                if self.shown.as_deref() != Some(pane.id.as_str()) {
                    let page = match &pane.source {
                        canvas_core::view::HtmlSource::Document(document) => document.as_str().to_owned(),
                        canvas_core::view::HtmlSource::File { path, .. } => canvas_core::report_html::notes::message_page(&format!("{path}: HTML notes are not shown on macOS yet.")),
                    };
                    self.load(&view, &page);
                    self.shown = Some(pane.id.clone());
                }
                if !self.visible {
                    view.setHidden(false);
                    self.visible = true;
                }
            }
        }
    }

    /// Gives the keyboard back to the canvas if the view has it.
    pub fn blur(&self) {
        let Some(view) = self.view() else { return };
        if let Some(window) = view.window() {
            let focused = window.firstResponder();
            // SAFETY: plain AppKit queries on this thread's views.
            let inside = focused.is_some_and(|r| unsafe {
                let is_view: bool = msg_send![&*r, isKindOfClass: NSView::class()];
                is_view && msg_send![&*(Retained::as_ptr(&r) as *const NSView), isDescendantOf: &**view]
            });
            if inside && self.parent != 0 {
                // SAFETY: the canvas window's live content view.
                let parent = unsafe { &*(self.parent as *const NSResponder) };
                window.makeFirstResponder(Some(parent));
            }
        }
    }

    /// Work that waits for the event loop (none yet).
    pub fn pump(&mut self) {}

    /// *Print / PDF* (not yet on macOS).
    pub fn print(&mut self) {
        self.state.borrow_mut().events.push(WebEvent::Notice("Printing an HTML pane is not available on macOS yet.".into(), true));
        wake();
    }

    fn load(&self, view: &ReportView, html: &str) {
        self.state.borrow_mut().expect = true;
        // SAFETY: a live view of this thread; WebKit copies the string.
        unsafe { view.loadHTMLString_baseURL(&NSString::from_str(html), None) };
    }

    fn hide(&mut self) {
        if !self.visible {
            return;
        }
        let Some(view) = self.view() else { return };
        view.setHidden(true);
        self.visible = false;
        // The keyboard goes back to the canvas if it was in the view.
        if let Some(window) = view.window() {
            let focused = window.firstResponder();
            // SAFETY: plain AppKit queries on this thread's views.
            let inside = focused.is_some_and(|r| unsafe {
                let is_view: bool = msg_send![&*r, isKindOfClass: NSView::class()];
                is_view && msg_send![&*(Retained::as_ptr(&r) as *const NSView), isDescendantOf: &**view]
            });
            if inside && self.parent != 0 {
                // SAFETY: the canvas window's live content view.
                let parent = unsafe { &*(self.parent as *const NSResponder) };
                window.makeFirstResponder(Some(parent));
            }
        }
    }

    /// Compiles the rule list, then creates the view (never without it).
    fn create(&mut self) {
        if self.parent == 0 {
            return;
        }
        self.state.borrow_mut().phase = Phase::Creating;
        if let Err(error) = std::fs::create_dir_all(&self.rules) {
            return fail(&self.state, format!("the report view cannot keep its rules: {error}"));
        }
        // SAFETY: WebKit's rule store in a folder of the canvas's.
        let store = unsafe { WKContentRuleListStore::storeWithURL(Some(&NSURL::fileURLWithPath(&NSString::from_str(&self.rules.display().to_string())))) };
        let Some(store) = store else { return fail(&self.state, "the report view cannot block network loads (no rule store)".into()) };
        let (weak, parent) = (Rc::downgrade(&self.state), self.parent);
        let block = RcBlock::new(move |list: *mut WKContentRuleList, error: *mut NSError| {
            let Some(state) = weak.upgrade() else { return };
            // SAFETY: WebKit hands a valid list or null.
            let Some(list) = (unsafe { Retained::retain(list) }) else {
                return fail(&state, format!("the report view cannot block network loads: {}", error_text(error)));
            };
            match build(&state, &list, parent) {
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

    /// The view's state for the test hooks.
    pub fn json(&self) -> Value {
        let s = self.state.borrow();
        // SAFETY: plain property reads on this thread's view.
        let (title, source) = s.view.as_ref().map_or((None, None), |v| unsafe { (v.title().map(|t| t.to_string()), v.URL().and_then(|u| u.absoluteString()).map(|u| clip(&u.to_string()))) });
        json!({
            "ok": true,
            "engine": "WKWebView",
            "phase": match &s.phase { Phase::Idle => "idle".to_owned(), Phase::Creating => "creating".into(), Phase::Ready => "ready".into(), Phase::Failed(why) => format!("failed: {why}") },
            "visible": self.visible,
            "bounds": self.bounds,
            "report": self.shown,
            "title": title,
            "source": source,
            "loads": s.loads,
            "blocked_navigations": s.blocked_navigations,
            "blocked_requests": s.requests,
            "blocked_request_uris": s.blocked_requests,
            "blocked_windows": s.windows,
            "blocked_downloads": 0,
            "denied_permissions": s.permissions,
            "folder": self.rules,
        })
    }

    /// Runs script in the page as the host, in the canvas's own world (test
    /// hooks only: the page's own scripts never run), and answers its result
    /// through `reply`.
    pub fn eval(&self, script: &str, reply: crossbeam_channel::Sender<Value>) {
        let Some(view) = self.view() else {
            let _ = reply.send(json!({ "ok": false, "error": "no HTML view" }));
            return;
        };
        let block = RcBlock::new(move |result: *mut AnyObject, error: *mut NSError| {
            // SAFETY: WebKit hands a valid result (or null) and error (or null).
            let answer = if error.is_null() { json!({ "ok": true, "result": unsafe { result.as_ref() }.map_or(Value::Null, json_of) }) } else { json!({ "ok": false, "error": error_text(error) }) };
            let _ = reply.send(answer);
        });
        // SAFETY: a live view of this thread; the block is reference counted.
        unsafe { view.evaluateJavaScript_inFrame_inContentWorld_completionHandler(&NSString::from_str(script), None, &WKContentWorld::defaultClientWorld(), Some(&block)) };
    }

    /// Loads HTML as it is, without the canvas's sanitizer (test hooks only:
    /// checks that the view's own lock-down holds by itself).
    pub fn probe_raw(&mut self, html: &str) -> Value {
        let Some(view) = self.view() else { return json!({ "ok": false, "error": "no HTML view" }) };
        self.load(&view, html);
        json!({ "ok": true })
    }

    /// Captures what the view shows as a PNG file (test hooks), answered through `reply`.
    pub fn capture(&self, path: std::path::PathBuf, reply: crossbeam_channel::Sender<Value>) {
        let Some(view) = self.view() else {
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

    /// Draws what the view showed after its latest load onto `png` (the
    /// window's own frame) where the view really is: the window as the window
    /// server composes it, for the test hooks.
    pub fn overlay(&self, png: &std::path::Path) -> anyhow::Result<()> {
        let (Some(view), true) = (self.view(), self.visible) else { return Ok(()) };
        let snapshot = self.state.borrow().snapshot.clone();
        let Some(snapshot) = snapshot else { return Ok(()) };
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

/// The locked-down configuration and the view, hidden in the canvas window.
fn build(state: &Rc<RefCell<State>>, rules: &WKContentRuleList, parent: isize) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("the HTML view needs the main thread")?;
    let weak = Rc::downgrade(state);
    // SAFETY: WebKit configuration calls on the main thread before the view exists.
    let view = unsafe {
        let configuration = WKWebViewConfiguration::new();
        configuration.setWebsiteDataStore(&WKWebsiteDataStore::nonPersistentDataStore());
        configuration.defaultWebpagePreferences().setAllowsContentJavaScript(false);
        let preferences = configuration.preferences();
        preferences.setJavaScriptCanOpenWindowsAutomatically(false);
        preferences.setFraudulentWebsiteWarningEnabled(false);
        configuration.setMediaTypesRequiringUserActionForPlayback(WKAudiovisualMediaTypes::WKAudiovisualMediaTypeAll);
        let content = configuration.userContentController();
        content.addContentRuleList(rules);
        let world = WKContentWorld::worldWithName(&NSString::from_str(WORLD));
        let counter = WKUserScript::initWithSource_injectionTime_forMainFrameOnly_inContentWorld(WKUserScript::alloc(), &NSString::from_str(COUNTER), WKUserScriptInjectionTime::AtDocumentStart, false, &world);
        content.addUserScript(&counter);
        let delegate = Delegate::new(weak.clone(), mtm);
        content.addScriptMessageHandler_contentWorld_name(ProtocolObject::from_ref(&*delegate), &world, &NSString::from_str("blocked"));
        let view = ReportView::new(weak, &configuration, mtm);
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
        state.borrow_mut().delegate = Some(delegate);
        view
    };
    state.borrow_mut().view = Some(view);
    Ok(())
}

impl Drop for ReportWeb {
    fn drop(&mut self) {
        let mut s = self.state.borrow_mut();
        if let Some(view) = s.view.take() {
            // SAFETY: this thread's view, taken out of the window it is in.
            unsafe {
                view.configuration().userContentController().removeAllScriptMessageHandlers();
                view.setNavigationDelegate(None);
                view.setUIDelegate(None);
                view.removeFromSuperview();
            }
        }
        s.delegate = None;
    }
}
