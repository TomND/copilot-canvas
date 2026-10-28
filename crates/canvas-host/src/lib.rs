//! L1 terminal host: hosted ConPTY terminals, their emulators, frames and
//! input encoding. Knows nothing about agents or the canvas.

pub use canvas_common::{paths, pipe, protocol, win};

pub mod keys;
pub mod server;
pub mod term;