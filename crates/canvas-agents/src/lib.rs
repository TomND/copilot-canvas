//! L3 canvas backend: everything that has to run where the agents are
//! (session history, agent CLIs, the event bus and passive sessions,
//! automations, VS Code storage). Runs inside the service process.

pub use canvas_common::{paths, protocol};
pub use canvas_sys::ipc as pipe;
pub use canvas_model::{bus, canvas_wide, doc, effects, prs, reviews};

pub mod agents;
pub mod automations;
pub mod backend;
pub mod cli;
pub mod copilot_cfg;
pub mod factory;
pub mod factory_cli;
pub mod fork;
pub mod git;
pub mod home_move;
pub mod index;
pub mod launcher;
pub mod migrate;
pub mod notebook;
pub mod notes_cli;
pub mod prs_cli;
pub mod remote;
pub mod reports;
pub mod reviews_cli;
pub mod service;
// Task Scheduler's COM API (Windows); every platform's backend is behind
// `scheduler::Scheduler` (MAC-LINUX-PLAN D7).
pub mod scheduler;
#[cfg(windows)]
pub mod taskschd;
pub mod todos_cli;
pub mod tunnel_access;
pub mod vscode;