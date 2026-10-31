//! The GUI's operating-system edges (MAC-LINUX-PLAN §4, §5.6): taskbar or
//! Dock state, flashing, toasts, the report's HTML view, clipboard images,
//! monitor work areas, process counters, icons, quiet test windows, the
//! native window handle, the cursor position and posted keys. Everything
//! else in the GUI is the same on every OS.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(not(windows))]
mod unix;
#[cfg(not(windows))]
pub use unix::*;
