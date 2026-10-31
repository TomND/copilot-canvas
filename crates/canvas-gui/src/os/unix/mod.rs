//! macOS and Linux: what winit and canvas-sys already give. Dock and
//! launcher state, notifications and the native HTML view come with Phase 4
//! of MAC-LINUX-PLAN; until then toasts are recorded but not shown and
//! reports use the formatted-text view (as on Windows without WebView2).

pub mod webview;

use std::time::Duration;

use winit::window::{Window, WindowAttributes};

pub fn in_front(_window: isize) -> bool {
    false
}

pub fn flash(_window: isize) {}

pub fn toast_thread_init() -> Result<(), String> {
    Err("notifications are not available on this platform yet".into())
}

pub fn show_toast(_title: &str, _body: &str) -> Result<(), String> {
    Err("notifications are not available on this platform yet".into())
}

pub fn toast_history_count() -> Option<u32> {
    None
}

/// Nothing to register outside Windows (the bundle identifier or the
/// `.desktop` file names the app).
pub fn set_app_id() {}

/// The Dock tile or launcher entry (not shown yet).
#[derive(Default)]
pub struct Taskbar {
    pub shown: Option<u8>,
}

impl Taskbar {
    pub fn set(&mut self, window: isize, state: u8) {
        if window != 0 {
            self.shown = Some(state);
        }
    }
}

/// Work areas are the monitors' full areas for now.
pub fn work_area(_monitor: &winit::monitor::MonitorHandle) -> Option<[i32; 4]> {
    None
}

/// Process CPU time and private memory.
pub fn process() -> (Duration, u64) {
    let usage = canvas_sys::process::process_usage(None);
    (Duration::from_millis(usage.cpu_ms), usage.private)
}

/// Time since this process was created.
pub fn since_start() -> Duration {
    let started = canvas_sys::process::process_start_ms(std::process::id()).unwrap_or(0);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    Duration::from_millis(now.saturating_sub(started))
}

/// The bundle's icon (macOS) or the `.desktop` file's (Linux) is used.
pub fn with_app_icon(attrs: WindowAttributes) -> WindowAttributes {
    attrs
}

/// A test run on someone's desktop: never take the focus.
pub fn quiet_window(attrs: WindowAttributes) -> WindowAttributes {
    attrs.with_active(false)
}

/// An image on the clipboard as RGBA.
pub fn clip_image() -> Option<(u32, u32, Vec<u8>)> {
    let png = canvas_sys::clipboard::get_image_png().ok()?;
    canvas_common::clipimage::decode_png(&png).ok()
}

/// The window's native handle: the NSView (macOS), the X11 window or the
/// Wayland surface (Linux).
pub fn native_window(window: &Window) -> isize {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::AppKit(h)) => h.ns_view.as_ptr() as isize,
        Ok(RawWindowHandle::Xlib(h)) => h.window as isize,
        Ok(RawWindowHandle::Xcb(h)) => h.window.get() as isize,
        Ok(RawWindowHandle::Wayland(h)) => h.surface.as_ptr() as isize,
        _ => 0,
    }
}

/// The pointer's position comes from winit's cursor events instead.
pub fn cursor_position(_window: &Window) -> Option<(f32, f32)> {
    None
}

/// Posted keys (latency benches) come with Phase 3.
pub fn post_key(_window: isize, _c: char) {}
