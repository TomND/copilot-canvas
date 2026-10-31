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
