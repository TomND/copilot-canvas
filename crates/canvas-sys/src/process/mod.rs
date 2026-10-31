//! Processes: liveness with start times (the guard against recycled pids),
//! the process table, trees, killing a tree, containment of descendants,
//! resource usage and the priority classes of the priority manager.
//!
//! Every tree walk keeps the rule that a child must have started after its
//! parent, so a recycled parent pid can never adopt unrelated processes.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

// The process table and per-process facts of each Unix.
#[cfg(macos)]
#[path = "macos.rs"]
mod os;
#[cfg(linux)]
#[path = "linux.rs"]
mod os;
#[cfg(macos)]
pub(crate) use os::all_pids;

/// How the priority manager treats a terminal's process tree
/// (README "The session you type in comes first").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Class {
    /// The session you type in.
    Focused,
    /// Sessions shown on screen.
    Visible,
    /// Every other session.
    Background,
}
