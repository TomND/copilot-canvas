//! The console of a test program that runs inside a canvas terminal (the
//! stand-in agent, the paste probe, the Kitty demo): raw VT input and VT
//! output, and a reader of standard input that hands every byte over
//! (Ctrl+Z and Ctrl+D included).

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;
