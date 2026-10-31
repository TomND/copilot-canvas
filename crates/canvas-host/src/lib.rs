//! L1 terminal host: hosted terminals (ConPTY on Windows, Unix PTYs on macOS
//! and Linux), their emulators, frames and input encoding. Knows nothing
//! about agents or the canvas.

pub use canvas_common::{paths, protocol};

pub mod emulator;
pub mod keys;
pub mod server;
pub mod term;