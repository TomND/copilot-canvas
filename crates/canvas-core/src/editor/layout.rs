//! A logical line laid out for drawing: its shown characters (raw, or
//! formatted in Live mode), their widths, and the soft-wrapped rows.

use ratatui::style::Style;

use super::markdown::{self, LineKind};
use super::text::char_width;

#[derive(Clone, Debug)]
pub(super) struct Layout {
    /// The wrap width it was made for.
    pub width: usize,
    /// The block kind it was formatted for (Live mode).
    pub kind: LineKind,
    pub chars: Vec<char>,
    pub widths: Vec<u8>,
    /// Raw column of each shown character (`None`: the same index).
    pub raw: Option<Vec<usize>>,
    /// Styles of the shown characters (formatted lines only).
    pub styles: Vec<Style>,
    /// Characters in the raw line.
    pub raw_len: usize,
    /// The first shown character of each visual row (`rows[0] == 0`).
    pub rows: Vec<usize>,
}

fn is_space(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// Soft wrap: rows break after spaces when possible, else at the width;
/// spaces may hang past the edge.
pub(super) fn wrap(chars: &[char], widths: &[u8], width: usize) -> Vec<usize> {
    let width = width.max(1);
    let mut rows = vec![0];
    let (mut start, mut x, mut soft) = (0usize, 0usize, None::<usize>);
    for (i, &c) in chars.iter().enumerate() {
        let w = widths[i] as usize;
        if w > 0 && !is_space(c) && x > 0 && x + w > width {
            let at = soft.filter(|&b| b > start && b <= i).unwrap_or(i);
            rows.push(at);
            start = at;
            soft = None;
            x = widths[at..i].iter().map(|w| *w as usize).sum();
            if x > 0 && x + w > width {
                rows.push(i);
                start = i;
                x = 0;
            }
        }
        x += w;
        if is_space(c) {
            soft = Some(i + 1);
        }
    }
    rows
}

impl Layout {
    /// `line` as it is (Source mode, or the cursor's line in Live mode).
    pub fn raw(line: &str, width: usize) -> Self {
        let chars: Vec<char> = line.chars().collect();
        let widths: Vec<u8> = chars.iter().map(|c| char_width(*c).min(255) as u8).collect();
        let rows = wrap(&chars, &widths, width);
        Self { width, kind: LineKind::Text, raw_len: chars.len(), chars, widths, raw: None, styles: Vec::new(), rows }
    }

    /// `line` formatted (Live mode).
    pub fn formatted(line: &str, kind: LineKind, width: usize) -> Self {
        let display = markdown::live(line, kind);
        let widths: Vec<u8> = display.chars.iter().map(|c| char_width(*c).min(255) as u8).collect();
        let rows = wrap(&display.chars, &widths, width);
        Self { width, kind, raw_len: line.chars().count(), chars: display.chars, widths, raw: Some(display.raw), styles: display.styles, rows }
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// The shown characters of row `row`: start and end indexes.
    pub fn row_range(&self, row: usize) -> (usize, usize) {
        let row = row.min(self.rows.len() - 1);
        (self.rows[row], self.rows.get(row + 1).copied().unwrap_or(self.chars.len()))
    }

    /// The row holding shown index `d` (the end belongs to the last row).
    pub fn row_of(&self, d: usize) -> usize {
        self.rows.partition_point(|&s| s <= d).saturating_sub(1)
    }

    /// The raw column of shown index `d` (the end maps to the line end).
    pub fn to_raw(&self, d: usize) -> usize {
        if d >= self.chars.len() {
            return self.raw_len;
        }
        self.raw.as_ref().map_or(d, |raw| raw[d])
    }

    /// The shown index of raw column `col` (the first character at or after it).
    pub fn to_shown(&self, col: usize) -> usize {
        match &self.raw {
            None => col.min(self.chars.len()),
            Some(raw) if col >= self.raw_len => raw.len(),
            Some(raw) => raw.partition_point(|&r| r < col),
        }
    }

    /// Cells from the start of its row to shown index `d`.
    pub fn x_of(&self, d: usize) -> usize {
        let d = d.min(self.chars.len());
        let start = self.rows[self.row_of(d)];
        self.widths[start..d].iter().map(|w| *w as usize).sum()
    }

    /// The shown index at `x` cells into row `row`: the character under it,
    /// or the row's end (before the break on a wrapped row).
    pub fn at_x(&self, row: usize, x: usize) -> usize {
        let (start, end) = self.row_range(row);
        let mut acc = 0;
        for i in start..end {
            let w = self.widths[i] as usize;
            if w > 0 && x < acc + w {
                return i;
            }
            acc += w;
        }
        if row + 1 < self.rows.len() && end > start { end - 1 } else { end }
    }
}
