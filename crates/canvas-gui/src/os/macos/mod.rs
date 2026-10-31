//! macOS: the WKWebView report view, the Dock tile and attention,
//! notifications, posted keys and work areas (MAC-LINUX-PLAN §5.6, §5.7).

pub mod access;
mod appkit;
mod dock;
mod menu;
mod notify;
pub mod webview;

pub use dock::{Taskbar, flash, in_front, track_window};
pub use notify::{show_toast, toast_history_count, toast_thread_init};

pub use super::unix::{clip_image, native_window, process, since_start};
pub use appkit::{post_chord, post_key, work_area};

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

/// How touchpad pixel scrolling is taken (MAC-LINUX-PLAN D15). macOS: the pointer policy (Precise).
pub fn scroll_policy() -> canvas_core::input::PointerPolicy {
    POINTER_POLICY
}

/// Which modifier is the app's (MAC-LINUX-PLAN D8).
pub const KEY_POLICY: canvas_core::input::KeyPolicy = canvas_core::input::KeyPolicy::CommandApp;

/// A quiet test run is an accessory app (no Dock icon, no menu bar, never
/// activated); otherwise a regular app.
pub fn configure_event_loop(builder: &mut winit::event_loop::EventLoopBuilder<crate::shell::UserEvent>, quiet_tests: bool) {
    use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
    builder.with_activation_policy(if quiet_tests { ActivationPolicy::Accessory } else { ActivationPolicy::Regular });
    // Ours instead (menu.rs): winit's Quit would end the app without asking.
    builder.with_default_menu(false);
    if quiet_tests {
        builder.with_activate_ignoring_other_apps(false);
    }
}

/// Nothing to register outside Windows (the bundle identifier or the
/// `.desktop` file names the app).
/// No helper process mode on macOS (the HTML view is a WKWebView in-process).
pub fn helper() -> Option<i32> {
    None
}

pub fn set_app_id() {}


/// Every window: the left Option key is Alt (Meta) for terminals, the right
/// one composes characters (MAC-LINUX-PLAN D8); the bundle's icon is used.
pub fn window_attributes(attrs: WindowAttributes) -> WindowAttributes {
    use winit::platform::macos::{OptionAsAlt, WindowAttributesExtMacOS};
    // The menu bar, once the app runs (the first window is being made).
    menu::install();
    attrs.with_option_as_alt(OptionAsAlt::OnlyLeft)
}

/// A test run on someone's desktop: never take the focus, but float above
/// other windows: macOS stops presenting to a window others cover (wgpu
/// reports it occluded), and the benches and parity checks count frames.
pub fn quiet_window(attrs: WindowAttributes) -> WindowAttributes {
    attrs.with_active(false).with_window_level(winit::window::WindowLevel::AlwaysOnTop)
}

/// The pointer's position comes from winit's cursor events instead.
/// Dropped files arrive as winit's `DroppedFile` here.
pub fn take_dropped(_window: &Window) -> Vec<String> {
    Vec::new()
}

pub fn cursor_position(_window: &Window) -> Option<(f32, f32)> {
    None
}

