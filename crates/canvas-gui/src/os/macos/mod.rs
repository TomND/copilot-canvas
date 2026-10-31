//! macOS: the WKWebView report view, posted keys and work areas; the Dock
//! tile, notifications and menus come here too (MAC-LINUX-PLAN §5.6, §5.7).

mod appkit;
pub mod webview;

pub use super::unix::{clip_image, native_window, process, since_start};
pub use appkit::{post_key, work_area};

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

/// How scrolling arrives (MAC-LINUX-PLAN D15): trackpad scrolls are
/// pixel-precise gestures (pinch, smart zoom and force click too); a mouse
/// wheel's line deltas stay wheel notches.
pub const POINTER_POLICY: canvas_core::input::PointerPolicy = canvas_core::input::PointerPolicy::Precise;

/// Which modifier is the app's (MAC-LINUX-PLAN D8).
pub const KEY_POLICY: canvas_core::input::KeyPolicy = canvas_core::input::KeyPolicy::CommandApp;

/// A quiet test run is an accessory app (no Dock icon, no menu bar, never
/// activated); otherwise a regular app.
pub fn configure_event_loop(builder: &mut winit::event_loop::EventLoopBuilder<crate::shell::UserEvent>, quiet_tests: bool) {
    use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
    builder.with_activation_policy(if quiet_tests { ActivationPolicy::Accessory } else { ActivationPolicy::Regular });
    if quiet_tests {
        builder.with_activate_ignoring_other_apps(false);
    }
}

pub fn in_front(_window: isize) -> bool {
    false
}

/// The windows and their focus, for `in_front` and `flash` where the OS
/// cannot be asked by handle (macOS asks AppKit: nothing to track).
pub fn track_window(_window: &std::sync::Arc<winit::window::Window>, _focused: bool) {}

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
/// No helper process mode on macOS (the HTML view is a WKWebView in-process).
pub fn helper() -> Option<i32> {
    None
}

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


/// Every window: the left Option key is Alt (Meta) for terminals, the right
/// one composes characters (MAC-LINUX-PLAN D8); the bundle's icon is used.
pub fn window_attributes(attrs: WindowAttributes) -> WindowAttributes {
    use winit::platform::macos::{OptionAsAlt, WindowAttributesExtMacOS};
    attrs.with_option_as_alt(OptionAsAlt::OnlyLeft)
}

/// A test run on someone's desktop: never take the focus, but float above
/// other windows: macOS stops presenting to a window others cover (wgpu
/// reports it occluded), and the benches and parity checks count frames.
pub fn quiet_window(attrs: WindowAttributes) -> WindowAttributes {
    attrs.with_active(false).with_window_level(winit::window::WindowLevel::AlwaysOnTop)
}

/// The pointer's position comes from winit's cursor events instead.
pub fn cursor_position(_window: &Window) -> Option<(f32, f32)> {
    None
}

