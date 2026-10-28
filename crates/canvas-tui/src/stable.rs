//! Terminal output that cannot drift.
//!
//! Terminals, Windows' console layer and ratatui do not always agree on how
//! many columns a character takes (emoji, symbols that gained an emoji form,
//! characters whose width changed in Unicode 16). When they disagree, every
//! following cell on that row lands one column off and stays wrong until it is
//! redrawn: "artifacts". This backend places the cursor explicitly after any
//! such character, so a disagreement can affect at most that one cell.

use std::io::{self, Write};

use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

/// True when every terminal draws this cell exactly one column wide.
pub fn stable_width(symbol: &str) -> bool {
    let mut chars = symbol.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        // Multi-codepoint graphemes (variation selectors, joiners, combining marks).
        return false;
    };
    let cp = c as u32;
    matches!(cp,
        0x20..=0x7E            // ASCII
        | 0xA0..=0xA8 | 0xAA..=0xAD | 0xAF..=0xFF // Latin-1 (© ® have emoji forms)
        | 0x100..=0x24F        // Latin extended
        | 0x2010..=0x2027 | 0x2030..=0x205E // punctuation (not zero-width/format chars)
        | 0x2190..=0x2193 | 0x2196..=0x21FF // arrows (↔ ↕ have emoji forms)
        | 0x2200..=0x22FF      // math operators
        | 0x2500..=0x25FC      // box drawing, blocks, geometric shapes (◽ ◾ are emoji)
        | 0x27C0..=0x27FF      // misc math / supplemental arrows (⟳)
        | 0x2800..=0x28FF      // braille (spinners)
        | 0x2900..=0x297F      // supplemental arrows (⤢)
        | 0x29C9               // ⧉
        | 0x2301 | 0x23FB      // ⌁ ⏻
        | 0x2713 | 0x2717 | 0x2726 // ✓ ✗ ✦
    )
}

pub struct StableBackend<W: Write> {
    inner: CrosstermBackend<W>,
}

impl<W: Write> StableBackend<W> {
    pub fn new(writer: W) -> Self {
        Self { inner: CrosstermBackend::new(writer) }
    }
}

impl<W: Write> Write for StableBackend<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Write::flush(&mut self.inner)
    }
}

impl<W: Write> Backend for StableBackend<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        // Each inner draw call starts with an explicit cursor move, so splitting
        // right after an uncertain cell re-anchors the next one.
        let mut batch: Vec<(u16, u16, &'a Cell)> = Vec::new();
        for item in content {
            let uncertain = !stable_width(item.2.symbol());
            batch.push(item);
            if uncertain {
                self.inner.draw(batch.drain(..))?;
            }
        }
        if !batch.is_empty() {
            self.inner.draw(batch.into_iter())?;
        }
        Ok(())
    }
    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }
    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.inner.get_cursor_position()
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }
    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }
    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }
    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }
    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }
    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    #[test]
    fn ui_glyphs_are_stable_and_emoji_are_not() {
        for s in ["a", " ", "─", "│", "╭", "▎", "◆", "●", "○", "■", "◷", "▤", "≡", "◈", "⇄", "⧉", "⌁", "✦", "✓", "✗", "⠋", "⤢", "⟳", "·", "…", "—", "é", "⏻"] {
            assert!(stable_width(s), "{s:?} should be stable");
        }
        for s in ["📢", "⚠\u{fe0f}", "☰", "✅", "👍🏽", "e\u{301}", "\u{200b}", "©", "↔", "◽", "中"] {
            assert!(!stable_width(s), "{s:?} should be re-anchored after");
        }
    }

    #[test]
    fn output_repositions_after_uncertain_cells() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 1));
        buf.set_string(0, 0, "ab📢cd⚠ef", ratatui::style::Style::default());
        let mut out: Vec<u8> = Vec::new();
        {
            let mut backend = StableBackend::new(&mut out);
            // As ratatui's diff does: the hidden half of a wide character is skipped.
            let cells: Vec<(u16, u16, &Cell)> = buf.content.iter().enumerate().filter(|(i, _)| *i != 3).map(|(i, c)| (i as u16, 0, c)).collect();
            backend.draw(cells.into_iter()).unwrap();
        }
        let text = String::from_utf8_lossy(&out);
        // Explicit cursor moves to the cells right after 📢 (col 5) and ⚠ (col 8).
        assert!(text.contains("\u{1b}[1;5Hcd"), "{text:?}");
        assert!(text.contains("\u{1b}[1;8Hef"), "{text:?}");
    }
}
