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
pub mod index;
pub mod migrate;
pub mod prs_cli;
pub mod remote;
pub mod reports;
pub mod reviews_cli;
pub mod service;
// Task Scheduler on Windows; macOS and Linux get their scheduler backends
// (launchd, systemd user timers, cron) behind a `Scheduler` trait in Phase 2
// of MAC-LINUX-PLAN; until then they have none.
#[cfg(windows)]
pub mod taskschd;
#[cfg(not(windows))]
#[path = "taskschd_none.rs"]
pub mod taskschd;
pub mod tunnel_access;
pub mod vscode;