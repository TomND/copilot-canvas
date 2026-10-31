//! The GUI's HTML view of reports: Microsoft Edge WebView2, hosted as a child
//! window of the canvas window over the report viewer's content area.
//! Agent-authored HTML is data, never code, and the view is locked down in
//! layers (the core's sanitizer already removed scripts, frames, plugins,
//! foreign URLs and event handlers, and put a strict Content-Security-Policy
//! first in the page):
//!
//! - scripts off (`IsScriptEnabled`), no web messages, no host objects, no
//!   script dialogs, devtools, context menus, status bar, zoom, browser
//!   accelerator keys, autofill or password saving;
//! - every network request is answered with 403 here (`WebResourceRequested`
//!   for `*`), downloads are cancelled, permissions denied, external URI
//!   schemes never launched;
//! - every navigation but the canvas's own load of the page is cancelled; a
//!   link the user clicks (and new-window requests) becomes a request to the
//!   canvas, which asks before opening it in the browser;
//! - its own user-data folder (`<home>/webview2`), never the user's Edge profile.
//!
//! When WebView2 cannot start (no runtime) the core shows the report as
//! formatted text and says why. The view never takes the keyboard on its own
//! (a click into it does; Esc in it closes the report).

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use canvas_core::view::HtmlPane;
use serde_json::{Value, json};
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    AcceleratorKeyPressedEventHandler, CapturePreviewCompletedHandler, CoTaskMemPWSTR, CoreWebView2EnvironmentOptions, CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, DownloadStartingEventHandler, ExecuteScriptCompletedHandler, LaunchingExternalUriSchemeEventHandler, NavigationCompletedEventHandler,
    NavigationStartingEventHandler, NewWindowRequestedEventHandler, PermissionRequestedEventHandler, ProcessFailedEventHandler, WebResourceRequestedEventHandler,
};
use windows::Win32::Foundation::{HWND, RECT};
use windows::core::{BOOL, Interface, PCWSTR, PWSTR};

/// Arguments for the view's browser process: no background traffic of its own.
const BROWSER_ARGS: &str = "--disable-background-networking --disable-component-update --disable-sync --no-first-run --disable-default-apps";

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
    Creating,
    Ready,
    Failed(String),
}

#[derive(Default)]
struct State {
    phase: Phase,
    environment: Option<ICoreWebView2Environment>,
    controller: Option<ICoreWebView2Controller>,
    webview: Option<ICoreWebView2>,
    /// The canvas's own next navigation (the report's page) may start.
    expect: bool,
    events: Vec<WebEvent>,
    /// What was refused (newest last, a few kept) and how often.
    blocked_navigations: Vec<String>,
    blocked_requests: Vec<String>,
    requests: u32,
    windows: u32,
    downloads: u32,
    permissions: u32,
    loads: u32,
}

/// One window's HTML view.
pub struct ReportWeb {
    state: Rc<RefCell<State>>,
    /// The view's own user-data folder.
    folder: std::path::PathBuf,
    /// The report whose page is loaded.
    shown: Option<String>,
    visible: bool,
    bounds: Option<[i32; 4]>,
    hwnd: isize,
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// A string a WebView2 getter hands out (freed here).
fn take_string(get: impl FnOnce(*mut PWSTR) -> windows::core::Result<()>) -> String {
    let mut raw = PWSTR::null();
    if get(&mut raw).is_err() {
        return String::new();
    }
    CoTaskMemPWSTR::from(raw).to_string()
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

fn describe(error: &windows::core::Error) -> String {
    // HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND): no runtime installed.
    if error.code().0 as u32 == 0x8007_0002 {
        "the Microsoft Edge WebView2 runtime is not installed".to_owned()
    } else {
        format!("WebView2 could not start: {}", error.message())
    }
}

impl ReportWeb {
    pub fn new(home: &std::path::Path) -> Self {
        ReportWeb { state: Rc::default(), folder: home.join("webview2"), shown: None, visible: false, bounds: None, hwnd: 0 }
    }

    /// What happened since the last call.
    pub fn take_events(&mut self) -> Vec<WebEvent> {
        std::mem::take(&mut self.state.borrow_mut().events)
    }

    /// Shows `pane` (an HTML report) over its area, or hides the view. The
    /// view is created the first time one is shown.
    pub fn sync(&mut self, hwnd: isize, pane: Option<&HtmlPane>, cell_w: f32, cell_h: f32) {
        self.hwnd = hwnd;
        let Some(pane) = pane else { return self.hide() };
        let phase = self.state.borrow().phase.clone();
        match phase {
            Phase::Idle => self.create(hwnd),
            Phase::Creating | Phase::Failed(_) => {}
            Phase::Ready => {
                let (controller, webview) = {
                    let s = self.state.borrow();
                    (s.controller.clone(), s.webview.clone())
                };
                let (Some(controller), Some(webview)) = (controller, webview) else { return };
                let r = pane.rect;
                let bounds = [
                    (r.x as f32 * cell_w).round() as i32,
                    (r.y as f32 * cell_h).round() as i32,
                    ((r.x + r.width) as f32 * cell_w).round() as i32,
                    ((r.y + r.height) as f32 * cell_h).round() as i32,
                ];
                if self.bounds != Some(bounds) {
                    // SAFETY: a live controller of this thread.
                    let _ = unsafe { controller.SetBounds(RECT { left: bounds[0], top: bounds[1], right: bounds[2], bottom: bounds[3] }) };
                    self.bounds = Some(bounds);
                }
                if self.shown.as_deref() != Some(pane.id.as_str()) {
                    self.state.borrow_mut().expect = true;
                    let page = wide(&pane.document);
                    // SAFETY: the page outlives the call (WebView2 copies it).
                    if let Err(error) = unsafe { webview.NavigateToString(PCWSTR(page.as_ptr())) } {
                        return fail(&self.state, format!("the report could not be shown: {}", error.message()));
                    }
                    self.shown = Some(pane.id.clone());
                }
                if !self.visible {
                    // SAFETY: as above.
                    let _ = unsafe { controller.SetIsVisible(true) };
                    self.visible = true;
                }
            }
        }
    }

    fn hide(&mut self) {
        if !self.visible {
            return;
        }
        let controller = self.state.borrow().controller.clone();
        if let Some(controller) = controller {
            // SAFETY: a live controller of this thread.
            let _ = unsafe { controller.SetIsVisible(false) };
        }
        self.visible = false;
        // The keyboard goes back to the canvas window if it was in the view.
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
        use windows_sys::Win32::UI::WindowsAndMessaging::IsChild;
        // SAFETY: plain Win32 calls on this thread's windows.
        unsafe {
            let focus = GetFocus();
            if !focus.is_null() && IsChild(self.hwnd as _, focus) != 0 {
                SetFocus(self.hwnd as _);
            }
        }
    }

    fn create(&mut self, hwnd: isize) {
        self.state.borrow_mut().phase = Phase::Creating;
        let _ = std::fs::create_dir_all(&self.folder);
        let state = self.state.clone();
        let options = CoreWebView2EnvironmentOptions::default();
        // SAFETY: set before the options are handed to WebView2.
        unsafe { options.set_additional_browser_arguments(BROWSER_ARGS.to_owned()) };
        let options: ICoreWebView2EnvironmentOptions = options.into();
        let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |result, environment| {
            match (result, environment) {
                (Ok(()), Some(environment)) => create_controller(state, environment, hwnd),
                (Err(error), _) => fail(&state, describe(&error)),
                (Ok(()), None) => fail(&state, "WebView2 gave no environment".into()),
            }
            Ok(())
        }));
        let folder = wide(&self.folder.display().to_string());
        // SAFETY: the folder string outlives the call; the handler is reference counted.
        let started = unsafe { CreateCoreWebView2EnvironmentWithOptions(PCWSTR::null(), PCWSTR(folder.as_ptr()), &options, &handler) };
        if let Err(error) = started {
            fail(&self.state, describe(&error));
        }
    }

    /// The view's state for the test hooks.
    pub fn json(&self) -> Value {
        let s = self.state.borrow();
        let title = s.webview.as_ref().map(|w| take_string(|p| unsafe { w.DocumentTitle(p) }));
        let source = s.webview.as_ref().map(|w| take_string(|p| unsafe { w.Source(p) }));
        json!({
            "ok": true,
            "phase": match &s.phase { Phase::Idle => "idle".to_owned(), Phase::Creating => "creating".into(), Phase::Ready => "ready".into(), Phase::Failed(why) => format!("failed: {why}") },
            "visible": self.visible,
            "bounds": self.bounds,
            "report": self.shown,
            "title": title,
            "source": source.map(|s| clip(&s)),
            "loads": s.loads,
            "blocked_navigations": s.blocked_navigations,
            "blocked_requests": s.requests,
            "blocked_request_uris": s.blocked_requests,
            "blocked_windows": s.windows,
            "blocked_downloads": s.downloads,
            "denied_permissions": s.permissions,
            "folder": self.folder,
        })
    }

    /// Runs script in the page as the host (test hooks only: the page's own
    /// scripts never run) and answers its JSON result through `reply`.
    pub fn eval(&self, script: &str, reply: crossbeam_channel::Sender<Value>) {
        let webview = self.state.borrow().webview.clone();
        let Some(webview) = webview else {
            let _ = reply.send(json!({ "ok": false, "error": "no HTML view" }));
            return;
        };
        let answer = reply.clone();
        let handler = ExecuteScriptCompletedHandler::create(Box::new(move |result, value| {
            let _ = answer.send(match result {
                Ok(()) => json!({ "ok": true, "result": serde_json::from_str::<Value>(&value).unwrap_or(Value::String(value)) }),
                Err(error) => json!({ "ok": false, "error": error.message() }),
            });
            Ok(())
        }));
        let script = wide(script);
        // SAFETY: the script outlives the call.
        if let Err(error) = unsafe { webview.ExecuteScript(PCWSTR(script.as_ptr()), &handler) } {
            let _ = reply.send(json!({ "ok": false, "error": error.message() }));
        }
    }

    /// Loads HTML as it is, without the canvas's sanitizer (test hooks only:
    /// checks that the view's own lock-down holds by itself).
    pub fn probe_raw(&mut self, html: &str) -> Value {
        let webview = self.state.borrow().webview.clone();
        let Some(webview) = webview else { return json!({ "ok": false, "error": "no HTML view" }) };
        self.state.borrow_mut().expect = true;
        let page = wide(html);
        // SAFETY: the page outlives the call.
        match unsafe { webview.NavigateToString(PCWSTR(page.as_ptr())) } {
            Ok(()) => json!({ "ok": true }),
            Err(error) => json!({ "ok": false, "error": error.message() }),
        }
    }

    /// Captures what the view shows as a PNG file (test hooks), answered through `reply`.
    pub fn capture(&self, path: std::path::PathBuf, reply: crossbeam_channel::Sender<Value>) {
        let webview = self.state.borrow().webview.clone();
        let Some(webview) = webview else {
            let _ = reply.send(json!({ "ok": false, "error": "no HTML view" }));
            return;
        };
        // SAFETY: an in-memory stream.
        let Some(stream) = (unsafe { windows::Win32::UI::Shell::SHCreateMemStream(None) }) else {
            let _ = reply.send(json!({ "ok": false, "error": "no memory stream" }));
            return;
        };
        let (answer, written) = (reply.clone(), stream.clone());
        let handler = CapturePreviewCompletedHandler::create(Box::new(move |result| {
            let outcome = result.map_err(|e| e.message()).and_then(|()| read_stream(&written).map_err(|e| e.message())).and_then(|bytes| std::fs::write(&path, &bytes).map(|()| bytes.len()).map_err(|e| e.to_string()));
            let _ = answer.send(match outcome {
                Ok(bytes) => json!({ "ok": true, "path": path, "bytes": bytes }),
                Err(error) => json!({ "ok": false, "error": error }),
            });
            Ok(())
        }));
        // SAFETY: the stream and handler are reference counted.
        if let Err(error) = unsafe { webview.CapturePreview(COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, &stream, &handler) } {
            let _ = reply.send(json!({ "ok": false, "error": error.message() }));
        }
    }
}

/// The window as the desktop composes it, child windows (the HTML view)
/// included, as a PNG (test hooks: `PrintWindow` with full content, which
/// works while the window is behind others). Answers its size.
pub fn capture_window(hwnd: isize, path: &std::path::Path) -> anyhow::Result<(u32, u32)> {
    use windows_sys::Win32::Foundation::RECT as SysRect;
    use windows_sys::Win32::Graphics::Gdi::{BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject};
    use windows_sys::Win32::Storage::Xps::PrintWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect;
    /// `PW_CLIENTONLY | PW_RENDERFULLCONTENT`.
    const FLAGS: u32 = 1 | 2;
    let mut rect = SysRect { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: GDI calls on this process's window; every object is released below.
    unsafe {
        GetClientRect(hwnd as _, &mut rect);
        let (width, height) = ((rect.right - rect.left).max(1), (rect.bottom - rect.top).max(1));
        let screen = GetDC(std::ptr::null_mut());
        let dc = CreateCompatibleDC(screen);
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader = BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: width, biHeight: -height, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB, ..std::mem::zeroed() };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        let old = SelectObject(dc, bitmap);
        let printed = PrintWindow(hwnd as _, dc, FLAGS) != 0;
        let mut rgba = Vec::new();
        if printed && !bits.is_null() {
            let bgra = std::slice::from_raw_parts(bits as *const u8, (width * height * 4) as usize);
            rgba = bgra.chunks(4).flat_map(|p| [p[2], p[1], p[0], 255]).collect();
        }
        SelectObject(dc, old);
        DeleteObject(bitmap);
        DeleteDC(dc);
        ReleaseDC(std::ptr::null_mut(), screen);
        anyhow::ensure!(printed, "PrintWindow failed");
        crate::gpu::write_png(path, width as u32, height as u32, &rgba)?;
        Ok((width as u32, height as u32))
    }
}

impl Drop for ReportWeb {
    fn drop(&mut self) {
        let controller = self.state.borrow_mut().controller.take();
        if let Some(controller) = controller {
            // SAFETY: closing a live controller of this thread.
            let _ = unsafe { controller.Close() };
        }
    }
}

/// The bytes of an in-memory stream.
fn read_stream(stream: &windows::Win32::System::Com::IStream) -> windows::core::Result<Vec<u8>> {
    use windows::Win32::System::Com::{STREAM_SEEK_END, STREAM_SEEK_SET};
    let mut size = 0u64;
    // SAFETY: a valid stream; the buffer is as large as the stream says.
    unsafe {
        stream.Seek(0, STREAM_SEEK_END, Some(&mut size))?;
        stream.Seek(0, STREAM_SEEK_SET, None)?;
        let mut bytes = vec![0u8; size as usize];
        let mut read = 0u32;
        stream.Read(bytes.as_mut_ptr().cast(), size as u32, Some(&mut read)).ok()?;
        bytes.truncate(read as usize);
        Ok(bytes)
    }
}

fn create_controller(state: Rc<RefCell<State>>, environment: ICoreWebView2Environment, hwnd: isize) {
    state.borrow_mut().environment = Some(environment.clone());
    let (done, env) = (state.clone(), environment.clone());
    let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |result, controller| {
        match (result, controller) {
            (Ok(()), Some(controller)) => match configure(&env, &controller, &done) {
                Ok(webview) => {
                    {
                        let mut s = done.borrow_mut();
                        s.controller = Some(controller);
                        s.webview = Some(webview);
                        s.phase = Phase::Ready;
                        s.events.push(WebEvent::Ready);
                    }
                    wake();
                }
                Err(error) => fail(&done, format!("the HTML view could not be set up: {}", error.message())),
            },
            (Err(error), _) => fail(&done, describe(&error)),
            (Ok(()), None) => fail(&done, "WebView2 gave no view".into()),
        }
        Ok(())
    }));
    // SAFETY: the canvas window is alive; the handler is reference counted.
    if let Err(error) = unsafe { environment.CreateCoreWebView2Controller(HWND(hwnd as *mut core::ffi::c_void), &handler) } {
        fail(&state, describe(&error));
    }
}

/// Locks the view down and listens to what it would do.
fn configure(environment: &ICoreWebView2Environment, controller: &ICoreWebView2Controller, state: &Rc<RefCell<State>>) -> windows::core::Result<ICoreWebView2> {
    let weak: Weak<RefCell<State>> = Rc::downgrade(state);
    let mut token = 0i64;
    // SAFETY: COM calls on this thread's (STA) view, as WebView2 requires.
    unsafe {
        controller.SetIsVisible(false)?;
        if let Ok(controller2) = controller.cast::<ICoreWebView2Controller2>() {
            controller2.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR { A: 255, R: 13, G: 17, B: 23 })?;
        }
        let webview = controller.CoreWebView2()?;
        let settings = webview.Settings()?;
        settings.SetIsScriptEnabled(false)?;
        settings.SetIsWebMessageEnabled(false)?;
        settings.SetAreDefaultScriptDialogsEnabled(false)?;
        settings.SetAreHostObjectsAllowed(false)?;
        settings.SetAreDevToolsEnabled(false)?;
        settings.SetAreDefaultContextMenusEnabled(false)?;
        settings.SetIsStatusBarEnabled(false)?;
        settings.SetIsZoomControlEnabled(false)?;
        settings.SetIsBuiltInErrorPageEnabled(false)?;
        if let Ok(settings) = settings.cast::<ICoreWebView2Settings3>() {
            settings.SetAreBrowserAcceleratorKeysEnabled(false)?;
        }
        if let Ok(settings) = settings.cast::<ICoreWebView2Settings4>() {
            settings.SetIsPasswordAutosaveEnabled(false)?;
            settings.SetIsGeneralAutofillEnabled(false)?;
        }
        if let Ok(settings) = settings.cast::<ICoreWebView2Settings5>() {
            settings.SetIsPinchZoomEnabled(false)?;
        }
        if let Ok(settings) = settings.cast::<ICoreWebView2Settings6>() {
            settings.SetIsSwipeNavigationEnabled(false)?;
        }

        // Only the canvas's own load of the page; a clicked link asks the canvas.
        let w = weak.clone();
        webview.add_NavigationStarting(
            &NavigationStartingEventHandler::create(Box::new(move |_, args| {
                let (Some(args), Some(state)) = (args, w.upgrade()) else { return Ok(()) };
                let uri = take_string(|p| args.Uri(p));
                let mut s = state.borrow_mut();
                if s.expect && (uri.starts_with("data:") || uri.starts_with("about:")) {
                    s.expect = false;
                    return Ok(());
                }
                args.SetCancel(true)?;
                let mut user = BOOL(0);
                let _ = args.IsUserInitiated(&mut user);
                push_capped(&mut s.blocked_navigations, clip(&uri));
                if user.as_bool() && canvas_core::reports_model::check_url(&uri).is_ok() {
                    s.events.push(WebEvent::Link(uri));
                    drop(s);
                    wake();
                }
                Ok(())
            })),
            &mut token,
        )?;
        let w = weak.clone();
        webview.add_NewWindowRequested(
            &NewWindowRequestedEventHandler::create(Box::new(move |_, args| {
                let (Some(args), Some(state)) = (args, w.upgrade()) else { return Ok(()) };
                // Handled without a window: none opens.
                args.SetHandled(true)?;
                let uri = take_string(|p| args.Uri(p));
                let mut user = BOOL(0);
                let _ = args.IsUserInitiated(&mut user);
                let mut s = state.borrow_mut();
                s.windows += 1;
                if user.as_bool() && canvas_core::reports_model::check_url(&uri).is_ok() {
                    s.events.push(WebEvent::Link(uri));
                    drop(s);
                    wake();
                }
                Ok(())
            })),
            &mut token,
        )?;
        // Nothing is fetched: every request is answered here with 403.
        webview.AddWebResourceRequestedFilter(windows::core::w!("*"), COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL)?;
        let (w, env) = (weak.clone(), environment.clone());
        webview.add_WebResourceRequested(
            &WebResourceRequestedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let response = env.CreateWebResourceResponse(None::<&windows::Win32::System::Com::IStream>, 403, windows::core::w!("Blocked by Copilot Canvas"), windows::core::w!(""))?;
                args.SetResponse(&response)?;
                if let Some(state) = w.upgrade() {
                    let uri = args.Request().map(|r| take_string(|p| r.Uri(p))).unwrap_or_default();
                    let mut s = state.borrow_mut();
                    s.requests += 1;
                    push_capped(&mut s.blocked_requests, clip(&uri));
                }
                Ok(())
            })),
            &mut token,
        )?;
        let w = weak.clone();
        webview.add_PermissionRequested(
            &PermissionRequestedEventHandler::create(Box::new(move |_, args| {
                if let Some(args) = args {
                    args.SetState(COREWEBVIEW2_PERMISSION_STATE_DENY)?;
                }
                if let Some(state) = w.upgrade() {
                    state.borrow_mut().permissions += 1;
                }
                Ok(())
            })),
            &mut token,
        )?;
        let w = weak.clone();
        webview.add_NavigationCompleted(
            &NavigationCompletedEventHandler::create(Box::new(move |_, _| {
                if let Some(state) = w.upgrade() {
                    state.borrow_mut().loads += 1;
                }
                Ok(())
            })),
            &mut token,
        )?;
        let w = weak.clone();
        webview.add_ProcessFailed(
            &ProcessFailedEventHandler::create(Box::new(move |_, _| {
                if let Some(state) = w.upgrade() {
                    fail(&state, "the HTML view's browser process ended".into());
                }
                Ok(())
            })),
            &mut token,
        )?;
        if let Ok(webview4) = webview.cast::<ICoreWebView2_4>() {
            let w = weak.clone();
            webview4.add_DownloadStarting(
                &DownloadStartingEventHandler::create(Box::new(move |_, args| {
                    if let Some(args) = args {
                        args.SetCancel(true)?;
                        args.SetHandled(true)?;
                    }
                    if let Some(state) = w.upgrade() {
                        state.borrow_mut().downloads += 1;
                    }
                    Ok(())
                })),
                &mut token,
            )?;
        }
        if let Ok(webview18) = webview.cast::<ICoreWebView2_18>() {
            webview18.add_LaunchingExternalUriScheme(
                &LaunchingExternalUriSchemeEventHandler::create(Box::new(move |_, args| {
                    if let Some(args) = args {
                        args.SetCancel(true)?;
                    }
                    Ok(())
                })),
                &mut token,
            )?;
        }
        // Esc in the view closes the report (the keyboard is the view's after a click into it).
        let w = weak.clone();
        controller.add_AcceleratorKeyPressed(
            &AcceleratorKeyPressedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let (mut key, mut kind) = (0u32, COREWEBVIEW2_KEY_EVENT_KIND::default());
                args.VirtualKey(&mut key)?;
                args.KeyEventKind(&mut kind)?;
                if key == 0x1b && kind == COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN {
                    args.SetHandled(true)?;
                    if let Some(state) = w.upgrade() {
                        state.borrow_mut().events.push(WebEvent::Escape);
                        wake();
                    }
                }
                Ok(())
            })),
            &mut token,
        )?;
        Ok(webview)
    }
}
