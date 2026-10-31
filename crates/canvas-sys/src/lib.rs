//! The platform layer (docs/MAC-LINUX-PLAN.md D1, §5.1): every operating
//! system call that is not GUI-specific, with the same API on Windows, macOS
//! and Linux. The Windows side re-exports today's `canvas_common::{win, pipe,
//! dpapi, auth, tcp}` functions unchanged, so Windows behaviour cannot change
//! by accident; macOS and Linux get their own implementations.
//!
//! `cfg` appears only on the `mod` declarations that pick an implementation;
//! code that must behave differently at run time asks [`host::platform`].

pub mod clipboard;
pub mod fs;
pub mod host;
pub mod ipc;
pub mod net;
pub mod open;
pub mod process;
pub mod secret;
pub mod spawn;
pub mod token;
