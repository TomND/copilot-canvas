//! Drawing: the visible rows with Markdown styling (Source mode, or the
//! formatted lines of Live mode), selection, find matches, the line-number
//! gutter, the placeholder and the caret.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use super::layout::Layout;
use super::markdown;
use super::{Editor, Pos, Shown, View};
use crate::paint::{ATTENTION, ATTENTION_BG, FAINT, GRAY, MUTED};

/// What one row needs besides its layout.
struct RowPaint<'a> {
    line: usize,
    /// Styles of the raw characters (raw layouts).
    styles: &'a [Style],
    selection: Option<(Pos, Pos)>,
    matches: &'a [(Pos, Pos)],
    x: u16,
    y: u16,
    width: usize,
}

fn put_cell(buf: &mut Buffer, x: u16, y: u16, symbol: &str, style: Style) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_symbol(symbol).set_style(style);
    }
}

/// Draws row `row` of `layout`; answers the cells it used.
fn paint_row(buf: &mut Buffer, layout: &Layout, row: usize, p: &RowPaint) -> usize {
    let (start, end) = layout.row_range(row);
    let mut used = 0usize;
    let mut tmp = [0u8; 4];
    for d in start..end {
        let w = layout.widths[d] as usize;
        if w == 0 {
            continue;
        }
        if used + w > p.width {
            break;
        }
        let raw = layout.to_raw(d);
        let mut style = if layout.raw.is_some() { layout.styles.get(d).copied().unwrap_or_default() } else { p.styles.get(raw).copied().unwrap_or_default() };
        let here = Pos::new(p.line, raw);
        let found = p.matches.iter().find(|(s, e)| *s <= here && here < *e);
        if let Some(m) = found {
            style = if Some(*m) == p.selection { style.fg(Color::Black).bg(ATTENTION) } else { style.bg(ATTENTION_BG) };
        } else if p.selection.is_some_and(|(s, e)| s <= here && here < e) {
            style = style.add_modifier(Modifier::REVERSED);
        }
        let c = layout.chars[d];
        let x = p.x.saturating_add(used as u16);
        if c == '\t' {
            for k in 0..w {
                put_cell(buf, x.saturating_add(k as u16), p.y, " ", style);
            }
        } else if c.is_control() {
            put_cell(buf, x, p.y, "·", style.fg(MUTED));
        } else {
            put_cell(buf, x, p.y, c.encode_utf8(&mut tmp), style);
            for k in 1..w {
                if let Some(cell) = buf.cell_mut((x.saturating_add(k as u16), p.y)) {
                    cell.reset();
                    cell.set_symbol("");
                }
            }
        }
        used += w;
    }
    // A selected line break shows as one selected cell after the text.
    let last_row = row + 1 == layout.row_count();
    let len = layout.raw_len;
    if last_row && used < p.width && p.selection.is_some_and(|(s, e)| s <= Pos::new(p.line, len) && Pos::new(p.line, len) < e) {
        put_cell(buf, p.x.saturating_add(used as u16), p.y, " ", Style::default().add_modifier(Modifier::REVERSED));
        used += 1;
    }
    used
}

impl Editor {
    pub(super) fn draw_into(&mut self, buf: &mut Buffer, area: Rect, focused: bool) -> Option<(u16, u16)> {
        let gutter = self.gutter_for(area.width);
        let width = area.width.saturating_sub(gutter) as usize;
        if self.view.area != area || self.view.gutter != gutter || self.view.width != width {
            self.view = View { area, gutter, width, shown: Vec::new() };
        }
        self.view.shown.clear();
        if area.width == 0 || area.height == 0 || width == 0 {
            return None;
        }
        // Blank the area (the caller's background stays).
        let visible = area.intersection(buf.area);
        for y in visible.top()..visible.bottom() {
            for x in visible.left()..visible.right() {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_symbol(" ");
                }
            }
        }
        self.trim_cache();
        let height = area.height as usize;
        self.reveal(height);
        let selection = self.selection();
        let empty = self.lines.len() == 1 && self.lines[0].is_empty();
        let text_x = area.x.saturating_add(gutter);
        let digits = gutter.saturating_sub(1) as usize;
        let mut caret = None;
        let (mut line, mut first_row) = self.top;
        let mut y = 0usize;
        while y < height && line < self.lines.len() {
            let layout = self.shown_layout(line);
            let styles = if layout.raw.is_none() {
                let kind = self.kind(line);
                markdown::source_styles(&self.lines[line], kind)
            } else {
                Vec::new()
            };
            let caret_at = (line == self.cursor.line).then(|| layout.to_shown(self.cursor.col));
            for row in first_row..layout.row_count() {
                if y >= height {
                    break;
                }
                let sy = area.y.saturating_add(y as u16);
                if gutter > 0 {
                    let label = if row == 0 { format!("{:>digits$} ", line + 1) } else { String::new() };
                    let style = Style::default().fg(if line == self.cursor.line { GRAY } else { FAINT });
                    crate::paint::put(buf, area.x as i32, sy as i32, &label, style, Rect::new(area.x, sy, gutter, 1));
                }
                if empty && !self.placeholder.is_empty() {
                    let text = crate::paint::truncate(&self.placeholder, width);
                    crate::paint::put(buf, text_x as i32, sy as i32, &text, Style::default().fg(MUTED), Rect::new(text_x, sy, width as u16, 1));
                } else {
                    let paint = RowPaint { line, styles: &styles, selection, matches: self.line_matches_of(line), x: text_x, y: sy, width };
                    paint_row(buf, &layout, row, &paint);
                }
                if let Some(d) = caret_at
                    && layout.row_of(d) == row
                {
                    let x = layout.x_of(d).min(width - 1);
                    caret = Some((text_x.saturating_add(x as u16), sy));
                }
                self.view.shown.push(Shown { line, row, layout: layout.clone() });
                y += 1;
            }
            first_row = 0;
            line += 1;
        }
        caret.filter(|_| focused)
    }
}
