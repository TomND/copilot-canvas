//! Linux: X11 and Wayland integration (launcher entry, freedesktop
//! notifications, the WebKitGTK report view, work areas) belongs here and to
//! the Linux lead (MAC-LINUX-PLAN §5.6, §5.7); until it lands, stand-ins.

pub use super::unix::{clip_image, native_window, process, since_start, webview};

/// wgpu's backends: Vulkan, GL as the fallback (llvmpipe and lavapipe are
/// the software adapters).
pub const GPU_BACKENDS: wgpu::Backends = wgpu::Backends::VULKAN.union(wgpu::Backends::GL);

/// Fonts: the system's (fontconfig folders) until the Linux lead picks named
/// files with fc-match (MAC-LINUX-PLAN §5.6).
pub mod fonts {
    pub use crate::os::font_sources::FontSources;

    pub fn sources() -> FontSources {
        FontSources { files: Vec::new(), dirs: Vec::new(), mono: &["Cascadia Mono", "DejaVu Sans Mono", "Noto Sans Mono", "Liberation Mono"], ui: &["Cantarell", "Noto Sans", "DejaVu Sans"] }
    }
}

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

/// The bundle's icon (macOS) or the `.desktop` file's (Linux) is used.
pub fn with_app_icon(attrs: WindowAttributes) -> WindowAttributes {
    attrs
}

/// A test run on someone's desktop: never take the focus.
pub fn quiet_window(attrs: WindowAttributes) -> WindowAttributes {
    attrs.with_active(false)
}

/// The pointer's position comes from winit's cursor events instead.
pub fn cursor_position(_window: &Window) -> Option<(f32, f32)> {
    None
}

/// Posted keys (latency benches) are not available yet.
pub fn post_key(_window: isize, _c: char) {}
