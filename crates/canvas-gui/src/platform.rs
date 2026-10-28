//! The GUI's platform: the Windows clipboard and link opening (core's
//! defaults), the taskbar button's progress (ITaskbarList3: busy while a
//! session works, paused while one needs you), flashing it (FlashWindowEx)
//! and Windows toasts for program notifications (OSC 777 / OSC 9) and
//! sessions that start to need you while the window is in the background,
//! and the window title.
//!
//! Toasts come from WinRT's `ToastNotificationManager`. An unpackaged app
//! needs a registered AppUserModelID: on first use the GUI registers
//! [`AUMID`] under `HKCU\Software\Classes\AppUserModelId` with its display
//! name (no Start-menu shortcut is needed) and shows toasts from a
//! background thread. When a toast cannot be shown the taskbar button still
//! flashes. One notification shows one toast, however many windows are open.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use canvas_core::platform::Platform;
use crossbeam_channel::{Sender, unbounded};

/// The app id toasts and the taskbar button use.
pub const AUMID: &str = "CopilotCanvas.Gui";
/// The same toast text is not shown twice within this time.
const DEDUPE: Duration = Duration::from_secs(3);

/// A toast the platform was asked for (the test hooks list them).
#[derive(Clone, Debug, serde::Serialize)]
pub struct ToastRecord {
    pub title: String,
    pub body: String,
    /// "queued", "shown", "in front" (the window was active: no toast),
    /// "duplicate" or "failed: …".
    pub status: String,
}

#[derive(Default)]
pub struct GuiPlatform {
    hwnd: AtomicIsize,
    /// OSC 9;4 state to show: 0 none, 3 busy, 4 paused (needs you).
    progress: AtomicU8,
    title: Mutex<String>,
    /// Progress or title changed since the UI thread last applied them.
    dirty: AtomicBool,
    pub bells: AtomicU64,
    pub flashes: AtomicU64,
    pub toasts: Arc<Mutex<Vec<ToastRecord>>>,
    /// Which window this is (0: the first; others say so in their title).
    pub index: AtomicUsize,
    /// With test hooks: what was copied and what the hooks put on the
    /// clipboard, used when Windows' clipboard is unavailable (a locked
    /// workstation refuses it).
    pub test_clipboard: Option<Arc<Mutex<TestClipboard>>>,
}

/// The test hooks' clipboard (see [`GuiPlatform::test_clipboard`]).
#[derive(Default, Debug)]
pub struct TestClipboard {
    pub text: Option<String>,
    pub image: Option<(u32, u32, Vec<u8>)>,
    /// Windows' clipboard refused the last access.
    pub system_unavailable: bool,
}

impl GuiPlatform {
    fn test_clip(&self) -> Option<std::sync::MutexGuard<'_, TestClipboard>> {
        self.test_clipboard.as_ref().map(|c| c.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// The image a paste would attach from the test hooks' clipboard.
    pub fn test_image(&self) -> Option<(u32, u32, Vec<u8>)> {
        self.test_clip().and_then(|c| c.image.clone())
    }
    pub fn set_window(&self, hwnd: isize) {
        self.hwnd.store(hwnd, Ordering::Release);
        self.dirty.store(true, Ordering::Release);
    }

    /// Applies title and taskbar changes (on the UI thread).
    pub fn apply(&self, window: &winit::window::Window, taskbar: &mut Taskbar) {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return;
        }
        let title = self.title.lock().map(|t| t.clone()).unwrap_or_default();
        if !title.is_empty() {
            window.set_title(&title);
        }
        taskbar.set(self.hwnd.load(Ordering::Acquire), self.progress.load(Ordering::Acquire));
    }

    pub fn current_title(&self) -> String {
        self.title.lock().map(|t| t.clone()).unwrap_or_default()
    }

    pub fn progress_state(&self) -> u8 {
        self.progress.load(Ordering::Acquire)
    }

    /// The window is the active one (no toast, no flashing).
    fn in_front(&self) -> bool {
        let hwnd = self.hwnd.load(Ordering::Acquire);
        // SAFETY: no arguments.
        hwnd != 0 && unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() } as isize == hwnd
    }

    fn flash(&self) {
        let hwnd = self.hwnd.load(Ordering::Acquire);
        if hwnd == 0 {
            return;
        }
        use windows_sys::Win32::UI::WindowsAndMessaging::{FLASHW_TIMERNOFG, FLASHW_TRAY, FLASHWINFO, FlashWindowEx};
        let info = FLASHWINFO { cbSize: std::mem::size_of::<FLASHWINFO>() as u32, hwnd: hwnd as _, dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG, uCount: 3, dwTimeout: 0 };
        // SAFETY: a valid FLASHWINFO for this process's window.
        unsafe { FlashWindowEx(&info) };
        self.flashes.fetch_add(1, Ordering::Relaxed);
    }

    /// Shows a toast unless the window is in front or the same text was just shown.
    fn toast(&self, title: String, body: String) {
        let status = if self.in_front() {
            "in front"
        } else if recently_shown(&title, &body) {
            "duplicate"
        } else {
            "queued"
        };
        let index = {
            let mut toasts = self.toasts.lock().unwrap_or_else(|p| p.into_inner());
            toasts.push(ToastRecord { title: title.clone(), body: body.clone(), status: status.into() });
            toasts.len() - 1
        };
        if status == "queued" {
            let _ = toaster().send(Job::Show { title, body, records: self.toasts.clone(), index });
        }
    }
}

impl Platform for GuiPlatform {
    fn set_clipboard_text(&self, text: &str) -> anyhow::Result<()> {
        let result = canvas_core::clipboard::set_text(text);
        match self.test_clip() {
            Some(mut clip) => {
                clip.text = Some(text.to_owned());
                clip.system_unavailable = result.is_err();
                Ok(())
            }
            None => result,
        }
    }

    fn clipboard_text(&self) -> anyhow::Result<String> {
        let system = canvas_core::clipboard::get_text();
        if let Some(clip) = self.test_clip()
            && (system.is_err() || clip.system_unavailable)
            && let Some(text) = &clip.text
        {
            return Ok(text.clone());
        }
        system
    }

    fn clipboard_has_text(&self) -> bool {
        self.test_clip().is_some_and(|c| c.system_unavailable && c.text.is_some()) || canvas_core::clipboard::has_text()
    }

    fn clipboard_has_image(&self) -> bool {
        self.test_clip().is_some_and(|c| c.image.is_some()) || canvas_core::clipboard::has_image()
    }

    fn bell(&self, _out: &mut Vec<u8>) {
        self.bells.fetch_add(1, Ordering::Relaxed);
        if !self.in_front() {
            self.flash();
        }
    }

    fn progress(&self, state: u8, _value: u8, _out: &mut Vec<u8>) {
        if self.progress.swap(state, Ordering::AcqRel) != state {
            self.dirty.store(true, Ordering::Release);
        }
    }

    fn title(&self, attention: usize) {
        let name = match self.index.load(Ordering::Relaxed) {
            0 => "Copilot Canvas".to_owned(),
            n => format!("Copilot Canvas · window {}", n + 1),
        };
        let title = match attention {
            0 => name,
            n => format!("({n}) {name} — {n} need{} you", if n == 1 { "s" } else { "" }),
        };
        if let Ok(mut current) = self.title.lock()
            && *current != title
        {
            *current = title;
            self.dirty.store(true, Ordering::Release);
        }
    }

    fn notification(&self, session: &str, title: &str, body: &str) {
        let heading = if title.trim().is_empty() { session.to_owned() } else { format!("{session}: {title}") };
        self.toast(heading, body.to_owned());
    }

    fn needs_you(&self, session: &str) {
        if !self.in_front() {
            self.flash();
        }
        self.toast(format!("“{session}” needs you"), "Waiting for your answer in Copilot Canvas.".into());
    }
}

/// The text was shown as a toast within [`DEDUPE`] (records it otherwise).
fn recently_shown(title: &str, body: &str) -> bool {
    static RECENT: Mutex<Vec<(String, Instant)>> = Mutex::new(Vec::new());
    let key = format!("{title}\n{body}");
    let mut recent = RECENT.lock().unwrap_or_else(|p| p.into_inner());
    recent.retain(|(_, at)| at.elapsed() < DEDUPE);
    if recent.iter().any(|(k, _)| *k == key) {
        return true;
    }
    recent.push((key, Instant::now()));
    false
}

enum Job {
    Show { title: String, body: String, records: Arc<Mutex<Vec<ToastRecord>>>, index: usize },
    History(Sender<Option<u32>>),
}

/// The toast thread (WinRT in the multithreaded apartment, off the UI thread).
fn toaster() -> &'static Sender<Job> {
    static TX: OnceLock<Sender<Job>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = unbounded::<Job>();
        let _ = std::thread::Builder::new().name("toasts".into()).spawn(move || {
            // SAFETY: initializes COM for this thread only.
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
            }
            let registered = register_aumid();
            for job in rx {
                match job {
                    Job::Show { title, body, records, index } => {
                        let status = match registered.clone().and_then(|_| show_toast(&title, &body).map_err(|e| e.message().to_string())) {
                            Ok(()) => "shown".to_owned(),
                            Err(error) => format!("failed: {error}"),
                        };
                        if let Some(record) = records.lock().unwrap_or_else(|p| p.into_inner()).get_mut(index) {
                            record.status = status;
                        }
                    }
                    Job::History(reply) => {
                        let _ = reply.send(history_count().ok());
                    }
                }
            }
        });
        tx
    })
}

/// Toasts of this app in the Action Center (`None` when it cannot be read).
pub fn toast_history() -> Option<u32> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    toaster().send(Job::History(tx)).ok()?;
    rx.recv_timeout(Duration::from_secs(5)).ok()?
}

fn history_count() -> windows::core::Result<u32> {
    use windows::UI::Notifications::ToastNotificationManager;
    ToastNotificationManager::History()?.GetHistoryWithId(&windows::core::HSTRING::from(AUMID))?.Size()
}

fn show_toast(title: &str, body: &str) -> windows::core::Result<()> {
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
    use windows::core::HSTRING;
    let xml = XmlDocument::new()?;
    xml.LoadXml(&HSTRING::from(format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        xml_escape(title),
        xml_escape(body)
    )))?;
    let toast = ToastNotification::CreateToastNotification(&xml)?;
    toast.SetGroup(&HSTRING::from("copilot-canvas"))?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?.Show(&toast)
}

fn xml_escape(text: &str) -> String {
    text.chars().take(400).fold(String::new(), |mut out, c| {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
        out
    })
}

/// Registers [`AUMID`] for toasts (per user; idempotent).
fn register_aumid() -> Result<(), String> {
    use windows_sys::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegSetValueExW};
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let path = wide(&format!(r"Software\Classes\AppUserModelId\{AUMID}"));
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: valid NUL-terminated strings; the key is closed below.
    let status = unsafe { RegCreateKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, std::ptr::null(), REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, std::ptr::null(), &mut key, std::ptr::null_mut()) };
    if status != 0 {
        return Err(format!("cannot register the app for toasts (error {status})"));
    }
    let name = wide("Copilot Canvas");
    let value = wide("DisplayName");
    // SAFETY: `name` is a NUL-terminated UTF-16 string of the given byte length.
    let status = unsafe { RegSetValueExW(key, value.as_ptr(), 0, REG_SZ, name.as_ptr().cast(), (name.len() * 2) as u32) };
    // SAFETY: opened above.
    unsafe { RegCloseKey(key) };
    if status != 0 { Err(format!("cannot register the app for toasts (error {status})")) } else { Ok(()) }
}

/// Gives the process the app id toasts are registered under (call before
/// the first window, so the taskbar button carries it too).
pub fn set_app_id() {
    // SAFETY: a static NUL-terminated string.
    let _ = unsafe { windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(windows::core::w!("CopilotCanvas.Gui")) };
}

/// The taskbar button's progress indicator.
#[derive(Default)]
pub struct Taskbar {
    list: Option<windows::Win32::UI::Shell::ITaskbarList3>,
    pub shown: Option<u8>,
}

impl Taskbar {
    fn set(&mut self, hwnd: isize, state: u8) {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
        use windows::Win32::UI::Shell::{ITaskbarList3, TBPF_INDETERMINATE, TBPF_NOPROGRESS, TBPF_PAUSED, TaskbarList};
        if hwnd == 0 || self.shown == Some(state) {
            return;
        }
        if self.list.is_none() {
            // SAFETY: COM on the UI thread (already initialized by winit; this is a no-op then).
            let list: Option<ITaskbarList3> = unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER).ok()
            };
            // SAFETY: HrInit before first use, as documented.
            self.list = list.filter(|l| unsafe { l.HrInit() }.is_ok());
        }
        let Some(list) = &self.list else { return };
        let hwnd = HWND(hwnd as *mut core::ffi::c_void);
        // SAFETY: a valid taskbar list and this process's window.
        unsafe {
            let _ = match state {
                3 => list.SetProgressState(hwnd, TBPF_INDETERMINATE),
                4 => list.SetProgressState(hwnd, TBPF_PAUSED).and_then(|_| list.SetProgressValue(hwnd, 100, 100)),
                _ => list.SetProgressState(hwnd, TBPF_NOPROGRESS),
            };
        }
        self.shown = Some(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_count_sessions_that_need_you() {
        let platform = GuiPlatform::default();
        platform.title(2);
        assert_eq!(*platform.title.lock().unwrap(), "(2) Copilot Canvas — 2 need you");
        assert!(platform.dirty.swap(false, Ordering::AcqRel));
        platform.title(2);
        assert!(!platform.dirty.load(Ordering::Acquire), "unchanged");
        platform.index.store(1, Ordering::Relaxed);
        platform.title(0);
        assert_eq!(platform.current_title(), "Copilot Canvas · window 2");
        let mut out = Vec::new();
        platform.progress(4, 100, &mut out);
        platform.bell(&mut out);
        assert!(out.is_empty(), "nothing goes to an outer terminal");
        assert_eq!((platform.bells.load(Ordering::Relaxed), platform.progress_state()), (1, 4));
    }

    #[test]
    fn the_same_toast_text_is_shown_once() {
        let title = format!("dedupe {}", std::process::id());
        assert!(!recently_shown(&title, "b"));
        assert!(recently_shown(&title, "b"));
        assert!(!recently_shown(&title, "other body"));
        assert_eq!(xml_escape("a<b>&\"c'\n"), "a&lt;b&gt;&amp;&quot;c&apos; ");
    }
}
