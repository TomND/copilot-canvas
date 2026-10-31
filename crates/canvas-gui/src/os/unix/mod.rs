//! What macOS and Linux share: process counters and the start time from
//! canvas-sys, clipboard images as PNG, the native window handle, and the
//! formatted-text report view used until (or when) the native HTML view is
//! unavailable.

// The formatted-text stand-in for the HTML view (macOS until its WKWebView;
// Linux has its own offscreen WebKit view).
#[cfg(target_os = "macos")]
pub mod webview;

use std::time::Duration;

use winit::window::Window;

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
