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
            Status::Working | Status::Running | Status::Preparing | Status::Resuming => TEXT,
            Status::Waiting => ATTENTION,
            Status::JustFinished => FINISHED,
            Status::Idle => GRAY,
            Status::External => PURPLE,
            Status::Stopped | Status::Unknown => DARK,
            // The failure colour: its program is gone mid-work (its glyph ↯ and border tell it from failed).
            Status::Failed | Status::Interrupted => NEGATIVE,
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

#[cfg(test)]
mod tests {
    #[test]
    fn the_interrupted_glyph_is_one_cell_wide_everywhere() {
        use unicode_width::UnicodeWidthStr;
        let glyph = super::INTERRUPTED_GLYPH;
        assert_eq!(glyph.width(), 1);
        assert_eq!(glyph.width_cjk(), 1, "not East Asian ambiguous (a console with a CJK width table would draw two cells)");
        assert_eq!(glyph.chars().count(), 1, "no variation selector (that asks for an emoji)");
        let cp = glyph.chars().next().unwrap() as u32;
        assert!(!(0x2630..=0x2637).contains(&cp), "not a glyph Unicode 16 widened");
    }
}
