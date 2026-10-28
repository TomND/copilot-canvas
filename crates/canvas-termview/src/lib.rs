//! L2 terminal view: the client-side model of a hosted terminal (built from
//! L1 frames), selection and link detection, plus drawing adapters.

pub mod tui;
mod view;

pub use view::{TermImage, TermSelection, TermView, user_message_rows};
