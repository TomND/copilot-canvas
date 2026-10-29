//! L3 canvas core: the app state machine (canvas, sidebar, dialogs, terminal
//! panes) shared by the front ends. Everything that touches the machine the
//! agents run on goes through the backend ([`rpc::Backend`]); the canvas
//! document is the service's and is kept in sync with patches ([`sync`]).

pub use canvas_common::{paths, pipe, protocol, settings, win};
pub use canvas_model::agent as agents;
pub use canvas_model::{automation, bus, config, doc, effects, git, migrate, rpc, sync, transcript, vscode};
pub use canvas_model::reports as reports_model;

pub mod actions;
pub mod app;
pub mod clipboard;
pub mod conn;
pub mod demo;
pub mod dialogs;
pub mod events;
pub mod factory_client;
pub mod files;
pub mod folders;
pub mod input;
pub mod inspect;
pub mod interact;
pub mod links;
pub mod loops;
pub mod modals;
pub mod models;
pub mod paint;
pub mod passive;
pub mod platform;
pub mod prefs;
pub mod recent;
pub mod remote;
pub mod remove;
pub mod report_html;
pub mod reports;
pub mod richtext;
pub mod side;
pub mod status;
pub mod tunnel;
pub mod ui;
pub mod ui_canvas;
pub mod ui_modals;
pub mod ui_side;
pub mod view;
pub mod viewer;
pub mod visual;
