//! macOS: the Dock tile, notifications, the WKWebView report view, menus
//! and posted keys come here (MAC-LINUX-PLAN §5.6, §5.7); until they land,
//! the same stand-ins as Linux.

pub use super::unix::{clip_image, native_window, process, since_start, webview};

/// wgpu's backend: Metal (macOS has no software adapter).
pub const GPU_BACKENDS: wgpu::Backends = wgpu::Backends::METAL;

/// Fonts: named files (no full scan, as on Windows): Cascadia Mono when the
/// user installed it, else SF Mono, Menlo; labels in SF; emoji, symbols and
/// CJK from Apple's fallback fonts (MAC-LINUX-PLAN §5.6).
pub mod fonts {
    use std::path::PathBuf;

    pub use crate::os::font_sources::FontSources;

    const SYSTEM_FILES: [&str; 13] = [
        "SFNSMono.ttf",
        "SFNSMonoItalic.ttf",
        "Menlo.ttc",
        "SFNS.ttf",
        "SFNSItalic.ttf",
        "Apple Color Emoji.ttc",
        "Apple Symbols.ttf",
        "AppleSDGothicNeo.ttc",
        "Hiragino Sans GB.ttc",
        "ヒラギノ角ゴシック W3.ttc",
        "ヒラギノ角ゴシック W6.ttc",
        "STHeiti Light.ttc",
        "CJKSymbolsFallback.ttc",
    ];

    pub fn sources() -> FontSources {
        let system = PathBuf::from("/System/Library/Fonts");
        let mut files: Vec<PathBuf> = SYSTEM_FILES.iter().map(|f| system.join(f)).collect();
        let user = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library").join("Fonts"));
        for dir in user.into_iter().chain([PathBuf::from("/Library/Fonts")]) {
            files.extend(["CascadiaMono.ttf", "CascadiaMonoItalic.ttf", "CascadiaMono-Regular.ttf"].iter().map(|f| dir.join(f)));
        }
        FontSources { files, dirs: Vec::new(), mono: &["Cascadia Mono", "SF Mono", ".SF NS Mono", "Menlo"], ui: &["SF Pro", "System Font", ".AppleSystemUIFont", "Helvetica Neue"] }
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
