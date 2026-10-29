//! The canvas data model shared by the backend (which owns the document and
//! answers requests) and the front ends (which draw and edit it): the
//! document and its patches, the event bus, and the data types of the
//! backend API.

pub use canvas_common::paths;

pub mod agent;
pub mod automation;
pub mod bus;
pub mod config;
pub mod doc;
pub mod git;
pub mod launch;
pub mod migrate;
pub mod rpc;
pub mod status;
pub mod sync;
pub mod vscode;
