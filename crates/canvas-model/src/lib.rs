//! The canvas document and event-bus data model shared by the backend
//! (which owns them) and the front ends (which draw and edit them).

pub use canvas_common::paths;

pub mod bus;
pub mod doc;