//! The GUI's operating-system edges (MAC-LINUX-PLAN §4, §5.6): taskbar or
//! Dock state, flashing, toasts, the report's HTML view, clipboard images,
//! monitor work areas, process counters, icons, quiet test windows, the
//! native window handle, the cursor position and posted keys. Everything
//! else in the GUI is the same on every OS.
//!
//! One module per OS, each with the same functions: `windows` (unchanged
//! Win32 code), `macos` (AppKit) and `linux` (X11 / Wayland, D-Bus); `unix`
//! holds what macOS and Linux share.

pub(crate) mod font_sources;

/// Input for an HTML view the GUI draws itself (Linux's offscreen WebKit:
/// MAC-LINUX-PLAN §5.7): what the window received while the pointer or the
/// keyboard was the view's. Positions are the window's physical pixels.
/// Views that are native child windows (WebView2, WKWebView) get their input
/// from the OS and ignore these.
// Read only by views the GUI draws itself.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub enum WebInput {
    /// A wheel or touchpad scroll: line steps or pixel deltas with the
    /// gesture's phase (touchpads: started, moved, ended; momentum follows).
    Wheel { x: f32, y: f32, delta: winit::event::MouseScrollDelta, phase: winit::event::TouchPhase },
    Motion { x: f32, y: f32 },
    Button { x: f32, y: f32, button: winit::event::MouseButton, pressed: bool },
    Key { key: winit::keyboard::Key, pressed: bool, mods: winit::keyboard::ModifiersState },
}

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(unix)]
mod unix;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;
