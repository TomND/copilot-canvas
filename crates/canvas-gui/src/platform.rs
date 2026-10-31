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
    /// Test runs on someone's desktop: toasts and taskbar flashes are only
    /// recorded (status `quiet`), never shown.
    pub quiet: bool,
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
    pub fn apply(&self, window: &winit::window::Window, taskbar: &mut crate::os::Taskbar) {
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
        crate::os::in_front(self.hwnd.load(Ordering::Acquire))
    }

    fn flash(&self) {
        let hwnd = self.hwnd.load(Ordering::Acquire);
        if hwnd == 0 {
            return;
        }
        if self.quiet {
            self.flashes.fetch_add(1, Ordering::Relaxed);
            return;
        }
        crate::os::flash(hwnd);
        self.flashes.fetch_add(1, Ordering::Relaxed);
    }

    /// Shows a toast unless the window is in front or the same text was just shown.
    fn toast(&self, title: String, body: String) {
        let status = if self.in_front() {
            "in front"
        } else if self.quiet {
            "quiet"
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
    /// The TUI in a new console window (Windows' default terminal).
    fn open_other_client(&self, args: &[String]) -> anyhow::Result<String> {
        let tui = canvas_core::platform::build_sibling(&canvas_sys::host::exe_name("copilot-canvas"))?;
        let mut all = vec!["--tui".to_owned()];
        all.extend(args.iter().cloned());
        canvas_core::platform::spawn_client(&tui, &all, true)?;
        Ok("Opened the TUI on this canvas in a new console window.".into())
    }

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

    fn report_arrived(&self, title: &str, source: &str) {
        if !self.in_front() {
            self.flash();
        }
        let body = if source.is_empty() { "In Copilot Canvas › Reports.".to_owned() } else { format!("From {source} · Copilot Canvas › Reports") };
        self.toast(format!("New report: {title}"), body);
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
            let registered = crate::os::toast_thread_init();
            for job in rx {
                match job {
                    Job::Show { title, body, records, index } => {
                        let status = match registered.clone().and_then(|_| crate::os::show_toast(&title, &body)) {
                            Ok(()) => "shown".to_owned(),
                            Err(error) => format!("failed: {error}"),
                        };
                        if let Some(record) = records.lock().unwrap_or_else(|p| p.into_inner()).get_mut(index) {
                            record.status = status;
                        }
                    }
                    Job::History(reply) => {
                        let _ = reply.send(crate::os::toast_history_count());
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
    }
}
