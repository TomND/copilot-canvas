//! User-facing status: the rules live in `canvas_model::status` (shared with
//! the iPhone client); this module adds the desktop palette.

use ratatui::style::Color;

pub use canvas_model::status::*;

use crate::paint::{ATTENTION, ATTENTION_BG, DARK, FINISHED, FINISHED_BG, GRAY, NEGATIVE, PURPLE, TEXT};

/// The desktop clients' colours for a [`Status`].
pub trait StatusStyle {
    fn color(self) -> Color;
    /// Border colour and compact-zoom background for states that want your eyes.
    fn attention(self) -> Option<(Color, Color)>;
}

impl StatusStyle for Status {
    fn color(self) -> Color {
        match self {
            Status::Working | Status::Running | Status::Preparing => TEXT,
            Status::Waiting => ATTENTION,
            Status::JustFinished => FINISHED,
            Status::Idle => GRAY,
            Status::External => PURPLE,
            Status::Stopped | Status::Unknown => DARK,
            Status::Failed => NEGATIVE,
        }
    }

    fn attention(self) -> Option<(Color, Color)> {
        match self {
            Status::Waiting => Some((ATTENTION, ATTENTION_BG)),
            Status::JustFinished => Some((FINISHED, FINISHED_BG)),
            _ => None,
        }
    }
}
