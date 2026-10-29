//! L3 canvas backend: everything that has to run where the agents are
//! (session history, agent CLIs, the event bus and passive sessions,
//! automations, VS Code storage). Runs inside the service process.

pub use canvas_common::{paths, pipe, protocol, win};
pub use canvas_model::{bus, canvas_wide, doc, effects};

pub mod agents;
pub mod automations;
pub mod backend;
pub mod cli;
pub mod clipboard;
pub mod copilot_cfg;
pub mod factory;
pub mod factory_cli;
pub mod fork;
pub mod git;
pub mod index;
pub mod migrate;
pub mod remote;
pub mod reports;
pub mod service;
pub mod taskschd;
pub mod vscode;