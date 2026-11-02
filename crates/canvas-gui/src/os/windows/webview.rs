//! The GUI's HTML view (reports, rendered Markdown notes, HTML notes):
//! Microsoft Edge WebView2, hosted as a child window of the canvas window
//! over the pane's area. Agent-authored HTML is data, never code, and the
//! view is locked down in layers (the core's sanitizer already removed
//! scripts, frames, plugins, foreign URLs and event handlers from the pages
//! it makes, and put a strict Content-Security-Policy first in them):
//!
//! - two views: the plain one never runs scripts (`IsScriptEnabled` off for
//!   good: reports, rendered Markdown, HTML notes without scripts); HTML notes
//!   allowed to run theirs get a second one (its own in-private profile when
//!   the runtime has profiles), whose page lives only while it is shown. In
//!   both: no web messages, host objects, script dialogs, devtools, context
//!   menus, status bar, zoom, browser accelerator keys, autofill or password
//!   saving;
//! - HTML notes are served from `https://notes.canvas.invalid/` mapped to the
//!   note's explorer root (`webpane::file_for`: nothing outside it; relative
//!   links resolve as on disk), and of that only the note's own folder and
//!   the folders below it (`webpane::classify`): each request is deferred,
//!   its bytes read through the backend on a worker thread (the service's
//!   machine, also remotely; at most `notebook::MAX_BYTES`; at most
//!   `webpane::MAX_PENDING` waiting, the rest 503), and answered back on
//!   this thread (WebView2 is single-threaded) with the note's policy as a
//!   header (`report_html::notes::csp`; the scripts one is a sandbox without
//!   `allow-same-origin`) and `nosniff`. The note itself is sanitized unless
//!   its scripts may run. A view whose page is replaced answers its waiting
//!   requests at once and skips their reads;
//! - every other request is answered with 403 here (`WebResourceRequested`
//!   for `*`, workers' requests too where the runtime can), downloads are
//!   cancelled, permissions denied, external URI schemes never launched;
//! - every navigation but the canvas's own load of the page is cancelled; a
//!   link the user clicks becomes a request to the canvas: http(s)/mailto
//!   asks before the browser, `#note:` / `#todo:` and links to other files
//!   under the note's root navigate the app (`webpane::link_action`); in-page
//!   `#fragment`s scroll as usual; an app link the page's own URL moves to
//!   counts in the scripts view only once per click into it
//!   (`webpane::AppLinks`; the window then takes the keyboard back);
//! - global keys (`webpane::web_key`) go to the canvas, the rest stays in
//!   the page; *Print / PDF* shows WebView2's print dialog;
//! - its own user-data folder (`<home>/webview2`), never the user's Edge
//!   profile, and a browser process without a network (`webpane::BROWSER_ARGS`).
//!
//! When WebView2 cannot start (no runtime) the core shows the page as
//! formatted text and says why. The view never takes the keyboard on its own
//! (a click into it does; Esc in it closes a report or gives the keyboard
//! back to the canvas).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use canvas_core::input::KeyEvent;
use canvas_core::rpc::Backend;
use canvas_core::spaces::{NavTarget, Space};
use canvas_core::view::{HtmlPane, HtmlSource};
use crossbeam_channel::{Receiver, Sender};
use serde_json::{Value, json};
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    AcceleratorKeyPressedEventHandler, CapturePreviewCompletedHandler, CoTaskMemPWSTR, CoreWebView2EnvironmentOptions, CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, DownloadStartingEventHandler, ExecuteScriptCompletedHandler, FocusChangedEventHandler, LaunchingExternalUriSchemeEventHandler,
    NavigationCompletedEventHandler, NavigationStartingEventHandler, NewWindowRequestedEventHandler, PermissionRequestedEventHandler, PrintToPdfCompletedHandler, ProcessFailedEventHandler,
    SourceChangedEventHandler, WebResourceRequestedEventHandler,
};
use windows::Win32::Foundation::{HWND, RECT};
use windows::core::{BOOL, Interface, PCWSTR, PWSTR};

use crate::webpane::{self, AppLinks, Expect, LinkAction, Reply, Resource, Serving, WebKey};

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

/// Which view: scripts never run in `Plain`; `Scripts` shows only HTML notes
/// allowed to run theirs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Plain = 0,
    Scripts = 1,
}

impl Role {
    fn other(self) -> Role {
        match self {
            Role::Plain => Role::Scripts,
            Role::Scripts => Role::Plain,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Role::Plain => "plain",
            Role::Scripts => "scripts",
        }
    }
}

/// What a view shows: the pane's id and, for an HTML note, what it serves.
type Shown = (String, Option<Serving>);

/// One of the two views.
#[derive(Default)]
struct Slot {
    /// The plain view's phase is also `State::phase`'s second half.
    phase: Phase,
    controller: Option<ICoreWebView2Controller>,
    webview: Option<ICoreWebView2>,
    /// The canvas's own next navigation (the page's load) may start.
    expect: Option<Expect>,
    /// The HTML note served (its explorer root is the origin's root; its folder is served).
    serving: Option<Serving>,
    shown: Option<Shown>,
    visible: bool,
    bounds: Option<[i32; 4]>,
    /// The page's own load in flight (its navigation id).
    loading: Option<u64>,
    /// Whether the view has the keyboard, and the app links its page's URL
    /// moved to (`SourceChanged`).
    links: AppLinks,
    /// That page's URL still names the link: loaded again when shown again.
    stale: bool,
    /// Its page's number: requests of a page that was replaced are not read.
    generation: Arc<AtomicU64>,
}

/// A request waiting for its file: answered on this thread when its reply
/// is made (or at once when its view's page is replaced).
struct Pending {
    role: Role,
    args: ICoreWebView2WebResourceRequestedEventArgs,
    deferral: ICoreWebView2Deferral,
}

/// A file to read through the backend and the reply to make of it (on a
/// reader thread: sanitizing a large note never stalls the window); `id`:
/// its pending request; skipped unless `live` is still `generation` (the page
/// that asked is still shown).
struct Job {
    id: u64,
    path: String,
    main: bool,
    scripts: bool,
    backend: Arc<dyn Backend>,
    live: Arc<AtomicU64>,
    generation: u64,
}

type Read = (u64, Reply);

#[derive(Default)]
struct State {
    /// The HTML view as a whole: ready once the plain view is; failed when
    /// the environment or the plain view failed.
    phase: Phase,
    environment: Option<ICoreWebView2Environment>,
    slots: [Slot; 2],
    events: Vec<WebEvent>,
    /// The backend files are read through (the window's, set every frame).
    backend: Option<Arc<dyn Backend>>,
    pending: HashMap<u64, Pending>,
    next_request: u64,
    jobs: Option<Sender<Job>>,
    reads: Option<(Sender<Read>, Receiver<Read>)>,
    /// *Print / PDF* asked for, waiting for the page to finish loading.
    print: bool,
    /// What was refused or served (newest last, a few kept) and how often.
    blocked_navigations: Vec<String>,
    blocked_requests: Vec<String>,
    served_requests: Vec<String>,
    requests: u32,
    served: u32,
    app_links: u32,
    windows: u32,
    downloads: u32,
    permissions: u32,
    loads: u32,
}

impl State {
    fn slot(&mut self, role: Role) -> &mut Slot {
        &mut self.slots[role as usize]
    }
}

/// One window's HTML view.
pub struct ReportWeb {
    state: Rc<RefCell<State>>,
    /// The view's own user-data folder.
    folder: std::path::PathBuf,
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

/// The HTML view as a whole cannot work (no runtime, the plain view or the
/// browser process failed): the core shows pages as formatted text.
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

fn fail_view(state: &Rc<RefCell<State>>, role: Role, why: String) {
    match role {
        Role::Plain => {
            state.borrow_mut().slot(Role::Plain).phase = Phase::Failed(why.clone());
            fail(state, why)
        }
        Role::Scripts => fail_scripts(state, why),
    }
}

fn describe(error: &windows::core::Error) -> String {
    // HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND): no runtime installed.
    if error.code().0 as u32 == 0x8007_0002 {
        "the Microsoft Edge WebView2 runtime is not installed".to_owned()
    } else {
        format!("WebView2 could not start: {}", error.message())
    }
}

fn phase_text(phase: &Phase) -> String {
    match phase {
        Phase::Idle => "idle".to_owned(),
        Phase::Creating => "creating".into(),
        Phase::Ready => "ready".into(),
        Phase::Failed(why) => format!("failed: {why}"),
    }
}

/// What a view loads for a pane.
enum Page {
    /// A whole page as a string (`NavigateToString`).
    Document(Arc<String>),
    /// An HTML note at its URL of the notes origin.
    File { url: String, serving: Serving },
}

/// Answers a request with `reply` and completes its deferral (if it was
/// deferred) whatever happened. Only requests of the notes origin are
/// deferred: should no response be made, the request goes on to a `.invalid`
/// host, which never resolves.
fn answer(environment: &ICoreWebView2Environment, args: &ICoreWebView2WebResourceRequestedEventArgs, deferral: Option<&ICoreWebView2Deferral>, reply: &Reply) {
    // SAFETY: COM calls on the thread that made the view (as WebView2 requires);
    // the stream copies the body; the strings outlive the calls.
    unsafe {
        let stream = if reply.body.is_empty() { None } else { windows::Win32::UI::Shell::SHCreateMemStream(Some(&reply.body)) };
        let (reason, headers) = (wide(reply.reason), wide(&reply.headers));
        if let Ok(response) = environment.CreateWebResourceResponse(stream.as_ref(), reply.status, PCWSTR(reason.as_ptr()), PCWSTR(headers.as_ptr())) {
            let _ = args.SetResponse(&response);
        }
        if let Some(deferral) = deferral {
            let _ = deferral.Complete();
        }
    }
}

/// Where a PDF goes when the runtime has no print dialog.
fn downloads() -> std::path::PathBuf {
    std::env::var_os("USERPROFILE").map(|home| std::path::PathBuf::from(home).join("Downloads")).filter(|dir| dir.is_dir()).unwrap_or_else(std::env::temp_dir)
}

/// A view's page is replaced: the requests it still waits on are answered
/// (403) now, not when their files come, and its reads not yet started are
/// skipped (the next page's requests do not queue behind them).
fn cancel(state: &Rc<RefCell<State>>, role: Role) {
    let (environment, stale) = {
        let mut s = state.borrow_mut();
        s.slot(role).generation.fetch_add(1, Ordering::Relaxed);
        let ids: Vec<u64> = s.pending.iter().filter(|(_, p)| p.role == role).map(|(id, _)| *id).collect();
        let stale: Vec<Pending> = ids.iter().filter_map(|id| s.pending.remove(id)).collect();
        (s.environment.clone(), stale)
    };
    if let Some(environment) = environment {
        for pending in stale {
            answer(&environment, &pending.args, Some(&pending.deferral), &webpane::refused());
        }
    }
}

impl ReportWeb {
    pub fn new(home: &std::path::Path) -> Self {
        ReportWeb { state: Rc::default(), folder: home.join("webview2"), hwnd: 0 }
    }

    /// What happened since the last call.
    pub fn take_events(&mut self) -> Vec<WebEvent> {
        std::mem::take(&mut self.state.borrow_mut().events)
    }

    /// The view's latest frame, for views the GUI draws itself (none here:
    /// this view is drawn by the OS).
    pub fn frame(&self) -> Option<crate::scene::ImageItem> {
        None
    }

    /// Input for views the GUI draws itself; this one gets its own from the
    /// OS, so nothing is taken.
    pub fn input(&mut self, _event: &crate::os::WebInput) -> bool {
        false
    }

    /// Shows `pane` over its area, or hides the views (`None`). A whole page
    /// (`HtmlSource::Document`: reports, rendered Markdown) loads as a string
    /// in the plain view; an HTML note (`HtmlSource::File`) from the notes
    /// origin, in the scripts view when its scripts may run (in the plain
    /// one, without them, if that view could not start). The views are
    /// created when first needed; `backend` reads the notes' files.
    pub fn sync(&mut self, hwnd: isize, pane: Option<&HtmlPane>, backend: &Arc<dyn Backend>, cell_w: f32, cell_h: f32) {
        self.hwnd = hwnd;
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
            Phase::Idle => return self.create(hwnd),
            Phase::Creating | Phase::Failed(_) => return,
            Phase::Ready => {}
        }
        let view_phase = self.state.borrow().slots[role as usize].phase.clone();
        match view_phase {
            // The plain view comes with the environment; the scripts view when first needed.
            Phase::Idle => create_controller(&self.state, role, hwnd),
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
                    Some(url) => (role, Page::File { url, serving: Serving { root: root.clone(), main: path.clone(), scripts: role == Role::Scripts } }),
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
        let (controller, webview) = {
            let s = self.state.borrow();
            let slot = &s.slots[role as usize];
            (slot.controller.clone(), slot.webview.clone())
        };
        let (Some(controller), Some(webview)) = (controller, webview) else { return };
        let r = pane.rect;
        let bounds = [
            (r.x as f32 * cell_w).round() as i32,
            (r.y as f32 * cell_h).round() as i32,
            ((r.x + r.width) as f32 * cell_w).round() as i32,
            ((r.y + r.height) as f32 * cell_h).round() as i32,
        ];
        let moved = self.state.borrow_mut().slot(role).bounds.replace(bounds) != Some(bounds);
        if moved {
            // SAFETY: a live controller of this thread.
            let _ = unsafe { controller.SetBounds(RECT { left: bounds[0], top: bounds[1], right: bounds[2], bottom: bounds[3] }) };
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
                slot.serving = serving;
                slot.shown = Some(shown);
                slot.stale = false;
                slot.loading = Some(0);
            }
            let started = match &page {
                Page::Document(document) => {
                    let page = wide(document);
                    // SAFETY: the page outlives the call (WebView2 copies it).
                    unsafe { webview.NavigateToString(PCWSTR(page.as_ptr())) }
                }
                Page::File { url, .. } => {
                    let url = wide(url);
                    // SAFETY: as above.
                    unsafe { webview.Navigate(PCWSTR(url.as_ptr())) }
                }
            };
            if let Err(error) = started {
                {
                    let mut s = self.state.borrow_mut();
                    let slot = s.slot(role);
                    (slot.expect, slot.shown, slot.serving, slot.loading) = (None, None, None, None);
                }
                return fail_view(&self.state, role, format!("the page could not be shown: {}", error.message()));
            }
        }
        if !self.state.borrow().slots[role as usize].visible {
            // SAFETY: a live controller of this thread.
            let _ = unsafe { controller.SetIsVisible(true) };
            self.state.borrow_mut().slot(role).visible = true;
        }
    }

    /// Hides a view. The scripts view also unloads its page: no note's
    /// script runs out of sight. The keyboard goes back to the canvas.
    fn hide(&mut self, role: Role) {
        let (visible, controller, webview) = {
            let s = self.state.borrow();
            let slot = &s.slots[role as usize];
            (slot.visible, slot.controller.clone(), slot.webview.clone())
        };
        if !visible {
            return;
        }
        if let Some(controller) = controller {
            // SAFETY: a live controller of this thread.
            let _ = unsafe { controller.SetIsVisible(false) };
        }
        {
            let mut s = self.state.borrow_mut();
            let slot = s.slot(role);
            slot.visible = false;
            if role == Role::Scripts {
                // Only its own about:blank may load now (its scripts still run until then).
                (slot.shown, slot.serving, slot.loading, slot.expect) = (None, None, None, Some(Expect::Blank));
            }
        }
        if role == Role::Scripts {
            cancel(&self.state, role);
            if let Some(webview) = webview {
                // SAFETY: a live view of this thread.
                let _ = unsafe { webview.Navigate(windows::core::w!("about:blank")) };
            }
        }
        self.blur();
    }

    /// Gives the keyboard back to the canvas window if a view has it.
    pub fn blur(&self) {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
        use windows_sys::Win32::UI::WindowsAndMessaging::IsChild;
        // SAFETY: plain Win32 calls on this thread's windows.
        let moved = unsafe {
            let focus = GetFocus();
            !focus.is_null() && IsChild(self.hwnd as _, focus) != 0 && !SetFocus(self.hwnd as _).is_null()
        };
        if moved {
            // Neither view has it now (their LostFocus says so too, later).
            for slot in self.state.borrow_mut().slots.iter_mut() {
                slot.links.focus(false);
            }
        }
    }

    fn create(&mut self, hwnd: isize) {
        self.state.borrow_mut().phase = Phase::Creating;
        let _ = std::fs::create_dir_all(&self.folder);
        let state = self.state.clone();
        let options = CoreWebView2EnvironmentOptions::default();
        // SAFETY: set before the options are handed to WebView2.
        unsafe { options.set_additional_browser_arguments(webpane::BROWSER_ARGS.to_owned()) };
        let options: ICoreWebView2EnvironmentOptions = options.into();
        let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |result, environment| {
            match (result, environment) {
                (Ok(()), Some(environment)) => {
                    state.borrow_mut().environment = Some(environment);
                    create_controller(&state, Role::Plain, hwnd);
                }
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

    /// Answers the requests whose files were read (here, on the thread that
    /// made the view, as WebView2 requires) and shows a print dialog that
    /// waited for its page.
    pub fn pump(&mut self) {
        let reads: Vec<Read> = self.state.borrow().reads.as_ref().map(|(_, rx)| rx.try_iter().collect()).unwrap_or_default();
        let environment = self.state.borrow().environment.clone();
        for (id, reply) in reads {
            let pending = self.state.borrow_mut().pending.remove(&id);
            if let (Some(pending), Some(environment)) = (pending, &environment) {
                answer(environment, &pending.args, Some(&pending.deferral), &reply);
            }
        }
        if self.state.borrow().print {
            self.run_print();
        }
    }

    /// *Print / PDF*: WebView2's print dialog for the pane's page (it also
    /// offers *Save as PDF*), once the page has loaded.
    pub fn print(&mut self) {
        self.state.borrow_mut().print = true;
        self.run_print();
    }

    fn run_print(&mut self) {
        let (webview, failed) = {
            let s = self.state.borrow();
            let shown = [Role::Plain, Role::Scripts].into_iter().find(|r| s.slots[*r as usize].visible);
            match (&s.phase, shown) {
                (Phase::Failed(why), _) => (None, Some(format!("Printing needs the HTML view: {why}."))),
                (_, Some(role)) if s.slots[role as usize].loading.is_none() => (s.slots[role as usize].webview.clone(), None),
                // Not shown or still loading: it waits (a pane that goes away cancels it).
                _ => return,
            }
        };
        self.state.borrow_mut().print = false;
        if let Some(why) = failed {
            return self.notice(why, true);
        }
        let Some(webview) = webview else { return };
        if let Ok(webview16) = webview.cast::<ICoreWebView2_16>() {
            // SAFETY: a live view of this thread.
            if let Err(error) = unsafe { webview16.ShowPrintUI(COREWEBVIEW2_PRINT_DIALOG_KIND_BROWSER) } {
                self.notice(format!("The print dialog could not open: {}", error.message()), true);
            }
            return;
        }
        self.save_pdf(&webview);
    }

    /// Without a print dialog (an older runtime): the page as a PDF in
    /// Downloads under a name claimed first (an empty file made only where
    /// none was: never over a file), then a notice says where. A PDF that
    /// could not be made leaves no empty file behind.
    fn save_pdf(&mut self, webview: &ICoreWebView2) {
        let Ok(webview7) = webview.cast::<ICoreWebView2_7>() else {
            return self.notice("Printing needs a newer Microsoft Edge WebView2 runtime.".into(), true);
        };
        // SAFETY: a live view of this thread.
        let title = take_string(|p| unsafe { webview.DocumentTitle(p) });
        let path = match webpane::pdf_path(&downloads(), &title, webpane::claim_new) {
            Ok(path) => path,
            Err(why) => return self.notice(format!("The PDF could not be saved: {why}"), true),
        };
        // Our claim, still empty: removed when no PDF was written into it.
        let unclaim = |path: &std::path::Path| {
            if std::fs::metadata(path).is_ok_and(|m| m.len() == 0) {
                let _ = std::fs::remove_file(path);
            }
        };
        let (weak, claimed, shown) = (Rc::downgrade(&self.state), path.clone(), path.display().to_string());
        let handler = PrintToPdfCompletedHandler::create(Box::new(move |result, saved| {
            let event = match (result, saved) {
                (Ok(()), true) => WebEvent::Notice(format!("Saved as PDF: {shown}"), false),
                (Err(error), _) => WebEvent::Notice(format!("The PDF could not be saved: {}", error.message()), true),
                (Ok(()), false) => WebEvent::Notice("The PDF could not be saved.".into(), true),
            };
            if matches!(event, WebEvent::Notice(_, true)) {
                unclaim(&claimed);
            }
            if let Some(state) = weak.upgrade() {
                state.borrow_mut().events.push(event);
                wake();
            }
            Ok(())
        }));
        let file = wide(&path.display().to_string());
        // SAFETY: the path outlives the call; the handler is reference counted.
        if let Err(error) = unsafe { webview7.PrintToPdf(PCWSTR(file.as_ptr()), None::<&ICoreWebView2PrintSettings>, &handler) } {
            unclaim(&path);
            self.notice(format!("The PDF could not be saved: {}", error.message()), true);
        }
    }

    fn notice(&mut self, text: String, error: bool) {
        self.state.borrow_mut().events.push(WebEvent::Notice(text, error));
        wake();
    }

    /// The view shown (else the plain one): the test hooks look at it.
    fn active(&self) -> Role {
        if self.state.borrow().slots[Role::Scripts as usize].visible { Role::Scripts } else { Role::Plain }
    }

    fn active_webview(&self) -> Option<ICoreWebView2> {
        let role = self.active();
        self.state.borrow().slots[role as usize].webview.clone()
    }

    /// The view's state for the test hooks.
    pub fn json(&self) -> Value {
        let role = self.active();
        let s = self.state.borrow();
        let slot = &s.slots[role as usize];
        // SAFETY: getters of a live view of this thread.
        let title = slot.webview.as_ref().map(|w| take_string(|p| unsafe { w.DocumentTitle(p) }));
        let source = slot.webview.as_ref().map(|w| take_string(|p| unsafe { w.Source(p) }));
        let views: Vec<Value> = [Role::Plain, Role::Scripts]
            .iter()
            .map(|r| {
                let v = &s.slots[*r as usize];
                let serving = v.serving.as_ref().map(|x| json!({ "root": x.root, "main": x.main, "scripts": x.scripts }));
                json!({ "view": r.name(), "phase": phase_text(&v.phase), "visible": v.visible, "shown": v.shown.as_ref().map(|(id, _)| id), "serving": serving, "loading": v.loading.is_some(), "focused": v.links.focused() })
            })
            .collect();
        json!({
            "ok": true,
            "phase": phase_text(&s.phase),
            "visible": slot.visible,
            "bounds": slot.bounds,
            "report": slot.shown.as_ref().map(|(id, _)| id),
            "view": role.name(),
            "views": views,
            "title": title,
            "source": source.map(|s| clip(&s)),
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
            "blocked_downloads": s.downloads,
            "denied_permissions": s.permissions,
            "folder": self.folder,
        })
    }

    /// Runs script in the page as the host (test hooks only: the plain
    /// view's pages never run their own) and answers its JSON result through `reply`.
    pub fn eval(&self, script: &str, reply: crossbeam_channel::Sender<Value>) {
        let Some(webview) = self.active_webview() else {
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

    /// Loads HTML as it is in the plain view, without the canvas's sanitizer
    /// (test hooks only: checks that the view's own lock-down holds by itself).
    pub fn probe_raw(&mut self, html: &str) -> Value {
        let webview = self.state.borrow().slots[Role::Plain as usize].webview.clone();
        let Some(webview) = webview else { return json!({ "ok": false, "error": "no HTML view" }) };
        cancel(&self.state, Role::Plain);
        {
            let mut s = self.state.borrow_mut();
            let slot = s.slot(Role::Plain);
            (slot.expect, slot.serving) = (Some(Expect::Page), None);
        }
        let page = wide(html);
        // SAFETY: the page outlives the call.
        match unsafe { webview.NavigateToString(PCWSTR(page.as_ptr())) } {
            Ok(()) => json!({ "ok": true }),
            Err(error) => json!({ "ok": false, "error": error.message() }),
        }
    }

    /// Captures what the view shows as a PNG file (test hooks), answered through `reply`.
    pub fn capture(&self, path: std::path::PathBuf, reply: crossbeam_channel::Sender<Value>) {
        let Some(webview) = self.active_webview() else {
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
        let (pending, environment, controllers) = {
            let mut s = self.state.borrow_mut();
            let controllers: Vec<ICoreWebView2Controller> = s.slots.iter_mut().filter_map(|slot| slot.controller.take()).collect();
            // Reads not yet started are skipped.
            for slot in &s.slots {
                slot.generation.fetch_add(1, Ordering::Relaxed);
            }
            (std::mem::take(&mut s.pending), s.environment.clone(), controllers)
        };
        // No request is left waiting.
        if let Some(environment) = &environment {
            for pending in pending.values() {
                answer(environment, &pending.args, Some(&pending.deferral), &webpane::refused());
            }
        }
        for controller in controllers {
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

impl State {
    /// Queues a file to read through the backend for request `id`'s reply
    /// (`false`: no backend or no reader thread; answer at once). The read is
    /// skipped if `role`'s page is replaced before it starts.
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

/// Starts the threads that read notes' files and make their replies
/// (`None`: none could start). Every job of a page still shown gets a
/// reply, so no request is left waiting (a replaced page's were answered
/// already: their jobs are dropped unread); they end when the view (the
/// jobs' sender) goes.
fn start_readers(done: Sender<Read>) -> Option<Sender<Job>> {
    let (jobs, queue) = crossbeam_channel::unbounded::<Job>();
    let mut started = 0;
    for n in 0..READERS {
        let (queue, done) = (queue.clone(), done.clone());
        let spawned = std::thread::Builder::new().name(format!("note-files-{n}")).spawn(move || {
            while let Ok(job) = queue.recv() {
                if job.live.load(Ordering::Relaxed) != job.generation {
                    // Its page was replaced and its request answered: nothing to read.
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

/// Creates a view (its controller): the scripts view in its own in-private
/// profile when the runtime has profiles.
fn create_controller(state: &Rc<RefCell<State>>, role: Role, hwnd: isize) {
    let Some(environment) = state.borrow().environment.clone() else { return };
    state.borrow_mut().slot(role).phase = Phase::Creating;
    let (done, env) = (state.clone(), environment.clone());
    let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |result, controller| {
        match (result, controller) {
            (Ok(()), Some(controller)) => match configure(&env, &controller, &done, role) {
                Ok(webview) => {
                    {
                        let mut s = done.borrow_mut();
                        let slot = s.slot(role);
                        (slot.controller, slot.webview, slot.phase) = (Some(controller), Some(webview), Phase::Ready);
                        if role == Role::Plain {
                            s.phase = Phase::Ready;
                            s.events.push(WebEvent::Ready);
                        }
                    }
                    wake();
                }
                Err(error) => {
                    // SAFETY: closing the controller that could not be locked down.
                    let _ = unsafe { controller.Close() };
                    fail_view(&done, role, format!("the HTML view could not be set up: {}", error.message()));
                }
            },
            (Err(error), _) => fail_view(&done, role, describe(&error)),
            (Ok(()), None) => fail_view(&done, role, "WebView2 gave no view".into()),
        }
        Ok(())
    }));
    let parent = HWND(hwnd as *mut core::ffi::c_void);
    // SAFETY: the canvas window is alive; the options and handler are reference counted.
    let started = unsafe {
        match (role, environment.cast::<ICoreWebView2Environment10>()) {
            (Role::Scripts, Ok(environment10)) => match environment10.CreateCoreWebView2ControllerOptions() {
                Ok(options) => {
                    let _ = options.SetProfileName(windows::core::w!("canvas-scripts"));
                    let _ = options.SetIsInPrivateModeEnabled(true);
                    environment10.CreateCoreWebView2ControllerWithOptions(parent, &options, &handler)
                }
                Err(_) => environment.CreateCoreWebView2Controller(parent, &handler),
            },
            _ => environment.CreateCoreWebView2Controller(parent, &handler),
        }
    };
    if let Err(error) = started {
        fail_view(state, role, describe(&error));
    }
}

/// Whether Ctrl and Shift are held (the key being handled is this thread's input).
fn held() -> (bool, bool) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_SHIFT};
    // SAFETY: plain Win32 calls.
    unsafe { (GetKeyState(VK_CONTROL as i32) < 0, GetKeyState(VK_SHIFT as i32) < 0) }
}

/// The character the keyboard layout gives a virtual key without modifiers.
fn layout_char(vk: u32) -> Option<char> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{MAPVK_VK_TO_CHAR, MapVirtualKeyW};
    // SAFETY: a plain Win32 call.
    let code = unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_CHAR) };
    char::from_u32(code & 0xFFFF).filter(|c| *c != '\0')
}

/// Locks a view down and listens to what it would do.
fn configure(environment: &ICoreWebView2Environment, controller: &ICoreWebView2Controller, state: &Rc<RefCell<State>>, role: Role) -> windows::core::Result<ICoreWebView2> {
    let weak: Weak<RefCell<State>> = Rc::downgrade(state);
    let mut token = 0i64;
    // SAFETY: COM calls on this thread's (STA) view, as WebView2 requires.
    unsafe {
        controller.SetIsVisible(false)?;
        if let Ok(controller2) = controller.cast::<ICoreWebView2Controller2>() {
            controller2.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR { A: 255, R: 13, G: 17, B: 23 })?;
        }
        if let Ok(controller4) = controller.cast::<ICoreWebView2Controller4>() {
            // A file dropped on the pane is neither opened (a `file:` navigation) nor handed to a page's script.
            let _ = controller4.SetAllowExternalDrop(false);
        }
        let webview = controller.CoreWebView2()?;
        let settings = webview.Settings()?;
        // Scripts run only in the scripts view, never in the plain one.
        settings.SetIsScriptEnabled(role == Role::Scripts)?;
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
        if let Ok(settings) = settings.cast::<ICoreWebView2Settings8>() {
            // No SmartScreen lookups: they would send the notes' URLs (their paths) away.
            settings.SetIsReputationCheckingRequired(false)?;
        }

        // Only the canvas's own load of the page; a clicked link asks the canvas.
        let w = weak.clone();
        webview.add_NavigationStarting(
            &NavigationStartingEventHandler::create(Box::new(move |sender, args| {
                let Some(args) = args else { return Ok(()) };
                let Some(state) = w.upgrade() else { return args.SetCancel(true) };
                let uri = take_string(|p| args.Uri(p));
                let current = sender.map(|webview| take_string(|p| webview.Source(p))).unwrap_or_default();
                let mut id = 0u64;
                let _ = args.NavigationId(&mut id);
                let mut user = BOOL(0);
                let _ = args.IsUserInitiated(&mut user);
                let mut s = state.borrow_mut();
                let slot = s.slot(role);
                let root = slot.serving.as_ref().map(|serving| serving.root.clone());
                if slot.expect.as_ref().is_some_and(|expect| webpane::expected(expect, &uri, root.as_deref())) {
                    (slot.expect, slot.loading) = (None, Some(id));
                    return Ok(());
                }
                // An in-page anchor scrolls (should this runtime announce it at
                // all); an app link in it is the canvas's (SourceChanged otherwise).
                if webpane::same_document(&current, &uri) && webpane::fragment_target(&uri).is_none() {
                    return Ok(());
                }
                args.SetCancel(true)?;
                // A link (a new document); not the history or a reload.
                let mut kind = COREWEBVIEW2_NAVIGATION_KIND_NEW_DOCUMENT;
                if let Ok(args3) = args.cast::<ICoreWebView2NavigationStartingEventArgs3>() {
                    let _ = args3.NavigationKind(&mut kind);
                }
                push_capped(&mut s.blocked_navigations, clip(&uri));
                if user.as_bool() && kind == COREWEBVIEW2_NAVIGATION_KIND_NEW_DOCUMENT {
                    asked(&mut s, &uri, root.as_deref());
                }
                Ok(())
            })),
            &mut token,
        )?;
        let w = weak.clone();
        webview.add_NewWindowRequested(
            &NewWindowRequestedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                // Handled without a window: none opens.
                args.SetHandled(true)?;
                let Some(state) = w.upgrade() else { return Ok(()) };
                let uri = take_string(|p| args.Uri(p));
                let mut user = BOOL(0);
                let _ = args.IsUserInitiated(&mut user);
                let mut s = state.borrow_mut();
                s.windows += 1;
                let root = s.slot(role).serving.as_ref().map(|serving| serving.root.clone());
                if user.as_bool() {
                    asked(&mut s, &uri, root.as_deref());
                }
                Ok(())
            })),
            &mut token,
        )?;
        // Every request comes here (also workers' where the runtime lets the
        // filter name them): files of the note's folder are read through the
        // backend (deferred, answered on this thread by `pump`), everything
        // else is answered with 403. Answered requests never reach the network.
        let all_sources = webview.cast::<ICoreWebView2_22>().is_ok_and(|webview22| {
            webview22.AddWebResourceRequestedFilterWithRequestSourceKinds(windows::core::w!("*"), COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL, COREWEBVIEW2_WEB_RESOURCE_REQUEST_SOURCE_KINDS_ALL).is_ok()
        });
        if !all_sources {
            webview.AddWebResourceRequestedFilter(windows::core::w!("*"), COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL)?;
        }
        let (w, env) = (weak.clone(), environment.clone());
        webview.add_WebResourceRequested(
            &WebResourceRequestedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let (uri, method) = match args.Request() {
                    Ok(request) => (take_string(|p| request.Uri(p)), take_string(|p| request.Method(p))),
                    Err(_) => (String::new(), String::new()),
                };
                let mut context = COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL;
                let _ = args.ResourceContext(&mut context);
                let Some(state) = w.upgrade() else {
                    answer(&env, &args, None, &webpane::refused());
                    return Ok(());
                };
                let (serving, admitted) = {
                    let s = state.borrow();
                    (s.slots[role as usize].serving.clone(), webpane::admit(s.pending.len()))
                };
                let resource = webpane::classify(&uri, &method, context == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT, serving.as_ref());
                // A file, while fewer than `MAX_PENDING` wait, and a deferral to wait with.
                let file = match resource {
                    Resource::File { path, main } if admitted => args.GetDeferral().ok().map(|deferral| (path, main, deferral)),
                    Resource::File { .. } => {
                        answer(&env, &args, None, &webpane::busy());
                        state.borrow_mut().requests += 1;
                        return Ok(());
                    }
                    Resource::Refused => None,
                };
                let Some((path, main, deferral)) = file else {
                    answer(&env, &args, None, &webpane::refused());
                    let mut s = state.borrow_mut();
                    s.requests += 1;
                    push_capped(&mut s.blocked_requests, clip(&uri));
                    return Ok(());
                };
                // The scripts policy and the unsanitized note only ever in the scripts view.
                let scripts = role == Role::Scripts;
                let mut s = state.borrow_mut();
                s.next_request += 1;
                let id = s.next_request;
                // The reply comes back through `pump`, on this thread, after this handler.
                if s.fetch(id, role, path.clone(), main, scripts) {
                    s.served += 1;
                    push_capped(&mut s.served_requests, clip(&uri));
                    s.pending.insert(id, Pending { role, args, deferral });
                } else {
                    drop(s);
                    answer(&env, &args, Some(&deferral), &webpane::reply(&path, main, scripts, Err("the files cannot be read now".into())));
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
            &NavigationCompletedEventHandler::create(Box::new(move |_, args| {
                let Some(state) = w.upgrade() else { return Ok(()) };
                let mut id = 0u64;
                if let Some(args) = &args {
                    let _ = args.NavigationId(&mut id);
                }
                let mut s = state.borrow_mut();
                s.loads += 1;
                let slot = s.slot(role);
                if slot.loading == Some(id) {
                    slot.loading = None;
                }
                if s.print {
                    // A print dialog waited for this page.
                    drop(s);
                    wake();
                }
                Ok(())
            })),
            &mut token,
        )?;
        let w = weak.clone();
        webview.add_ProcessFailed(
            &ProcessFailedEventHandler::create(Box::new(move |_, _| {
                if let Some(state) = w.upgrade() {
                    match role {
                        Role::Plain => fail(&state, "the HTML view's browser process ended".into()),
                        Role::Scripts => fail_scripts(&state, "its browser process ended".into()),
                    }
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
        // Global keys go to the canvas (the keyboard is the view's after a click into it).
        let w = weak.clone();
        controller.add_AcceleratorKeyPressed(
            &AcceleratorKeyPressedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let (mut key, mut kind) = (0u32, COREWEBVIEW2_KEY_EVENT_KIND::default());
                args.VirtualKey(&mut key)?;
                args.KeyEventKind(&mut kind)?;
                if kind != COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN && kind != COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_DOWN {
                    return Ok(());
                }
                let mut status = COREWEBVIEW2_PHYSICAL_KEY_STATUS::default();
                let _ = args.PhysicalKeyStatus(&mut status);
                let (ctrl, shift) = held();
                let Some(action) = webpane::web_key(key, layout_char(key), ctrl, shift, status.IsMenuKeyDown.as_bool()) else { return Ok(()) };
                args.SetHandled(true)?;
                if let Some(state) = w.upgrade() {
                    state.borrow_mut().events.push(match action {
                        WebKey::Escape => WebEvent::Escape,
                        WebKey::Forward(key) => WebEvent::Key(key),
                    });
                    wake();
                }
                Ok(())
            })),
            &mut token,
        )?;
        // `#note:` / `#todo:` links are same-document navigations (no
        // NavigationStarting): the page's URL names the place afterwards.
        let w = weak.clone();
        webview.add_SourceChanged(
            &SourceChangedEventHandler::create(Box::new(move |sender, args| {
                let (Some(webview), Some(args), Some(state)) = (sender, args, w.upgrade()) else { return Ok(()) };
                let mut new_document = BOOL(0);
                let _ = args.IsNewDocument(&mut new_document);
                if new_document.as_bool() {
                    return Ok(());
                }
                let source = take_string(|p| webview.Source(p));
                let Some((space, target)) = webpane::fragment_target(&source) else { return Ok(()) };
                let mut s = state.borrow_mut();
                let slot = s.slot(role);
                // In the scripts view once per click into it (the window then takes the keyboard back).
                let visible = slot.visible;
                if slot.links.follow(role == Role::Scripts, visible, Instant::now()) {
                    slot.stale = true;
                    s.app_links += 1;
                    s.events.push(WebEvent::Navigate(space, target));
                    drop(s);
                    wake();
                } else {
                    push_capped(&mut s.blocked_navigations, clip(&source));
                }
                Ok(())
            })),
            &mut token,
        )?;
        for got in [true, false] {
            let w = weak.clone();
            let handler = FocusChangedEventHandler::create(Box::new(move |_, _| {
                if let Some(state) = w.upgrade() {
                    state.borrow_mut().slot(role).links.focus(got);
                }
                Ok(())
            }));
            if got {
                controller.add_GotFocus(&handler, &mut token)?;
            } else {
                controller.add_LostFocus(&handler, &mut token)?;
            }
        }
        Ok(webview)
    }
}

/// A link the user followed (cancelled in the view): the canvas acts on it.
fn asked(s: &mut std::cell::RefMut<'_, State>, uri: &str, root: Option<&str>) {
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
