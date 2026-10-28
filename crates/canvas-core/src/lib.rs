//! L3 canvas core: the app state machine (canvas, sidebar, dialogs, terminal
//! panes) shared by the front ends.

pub use canvas_agents::{automations, copilot_cfg, factory, factory_cli, fork, git, index, migrate, taskschd, vscode};
pub use canvas_common::{paths, pipe, protocol, win};
pub use canvas_model::{bus, doc};

pub mod actions;
pub mod app;
pub mod clipboard;
pub mod conn;
pub mod dialogs;
pub mod events;
pub mod factory_client;
pub mod input;
pub mod links;
pub mod modals;
pub mod paint;
pub mod passive;
pub mod side;
pub mod status;
pub mod ui;
pub mod ui_canvas;
pub mod ui_modals;
pub mod ui_side;