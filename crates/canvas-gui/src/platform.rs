//! The GUI's platform: the Windows clipboard and link opening (core's
//! defaults), the taskbar button's progress (ITaskbarList3) and flashing for
//! bells (FlashWindowEx), and the window title.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, Ordering};

use canvas_core::platform::Platform;

#[derive(Default)]
pub struct GuiPlatform {
    hwnd: AtomicIsize,
    /// OSC 9;4 state to show: 0 none, 3 busy, 4 paused (needs you).
    progress: AtomicU8,
    title: Mutex<String>,
    /// Progress or title changed since the UI thread last applied them.
    dirty: AtomicBool,
    pub bells: std::sync::atomic::AtomicU64,
}

impl GuiPlatform {
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
}

impl Platform for GuiPlatform {
    fn bell(&self, _out: &mut Vec<u8>) {
        self.bells.fetch_add(1, Ordering::Relaxed);
        let hwnd = self.hwnd.load(Ordering::Acquire);
        if hwnd == 0 {
            return;
        }
        use windows_sys::Win32::UI::WindowsAndMessaging::{FLASHW_TIMERNOFG, FLASHW_TRAY, FLASHWINFO, FlashWindowEx};
        let info = FLASHWINFO { cbSize: std::mem::size_of::<FLASHWINFO>() as u32, hwnd: hwnd as _, dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG, uCount: 3, dwTimeout: 0 };
        // SAFETY: a valid FLASHWINFO for this process's window.
        unsafe { FlashWindowEx(&info) };
    }

    fn progress(&self, state: u8, _value: u8, _out: &mut Vec<u8>) {
        if self.progress.swap(state, Ordering::AcqRel) != state {
            self.dirty.store(true, Ordering::Release);
        }
    }

    fn title(&self, attention: usize) {
        let title = match attention {
            0 => "Copilot Canvas".to_owned(),
            n => format!("({n}) Copilot Canvas — {n} need{} you", if n == 1 { "s" } else { "" }),
        };
        if let Ok(mut current) = self.title.lock()
            && *current != title
        {
            *current = title;
            self.dirty.store(true, Ordering::Release);
        }
    }
}

/// The taskbar button's progress indicator.
#[derive(Default)]
pub struct Taskbar {
    list: Option<windows::Win32::UI::Shell::ITaskbarList3>,
    shown: Option<u8>,
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
        let mut out = Vec::new();
        platform.progress(4, 100, &mut out);
        platform.bell(&mut out);
        assert!(out.is_empty(), "nothing goes to an outer terminal");
        assert_eq!(platform.bells.load(Ordering::Relaxed), 1);
    }
}
