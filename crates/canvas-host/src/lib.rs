//! L1 terminal host: hosted ConPTY terminals, their emulators, frames and
//! input encoding. Knows nothing about agents or the canvas.

pub use canvas_common::{paths, protocol};
#[cfg(windows)]
pub use canvas_common::{pipe, win};

pub mod emulator;
pub mod keys;
// ConPTY terminals and the service are Windows-only; the emulators and the
// key encoder also serve the macOS mock service (crates/canvas-mobile).
#[cfg(windows)]
pub mod server;
#[cfg(windows)]
pub mod term;