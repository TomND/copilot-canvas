//! TUI adapter: draws a [`TermView`] into a ratatui buffer region.

use canvas_common::protocol::{COLOR_DEFAULT, COLOR_RGB, F_BOLD, F_DIM, F_HIDDEN, F_INVERSE, F_ITALIC, F_STRIKE, F_UNDERLINE};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthChar;

use crate::{TermSelection, TermView, user_message_rows};

pub struct Options<'a> {
    /// Highlight your own messages (Copilot's " ❯ " rows): (background, bar colour).
    pub tint: Option<(Color, Color)>,
    pub selection: Option<&'a TermSelection>,
    /// Report the cursor position (focused, live terminal).
    pub cursor: bool,
}

#[derive(Default)]
pub struct Drawn {
    /// OSC 8 hyperlinks: screen cells and target.
    pub links: Vec<(Rect, String)>,
    pub cursor: Option<(u16, u16)>,
}

/// Maps a protocol colour to a terminal colour.
pub fn color(value: u32) -> Color {
    match value {
        COLOR_DEFAULT => Color::Reset,
        v if v & COLOR_RGB != 0 => Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8),
        v => Color::Indexed((v - 1) as u8),
    }
}

pub fn style(fg: u32, bg: u32, flags: u16, link: bool) -> Style {
    let mut style = Style::default().fg(color(fg)).bg(color(bg));
    let mods = [
        (F_BOLD, Modifier::BOLD),
        (F_ITALIC, Modifier::ITALIC),
        (F_UNDERLINE, Modifier::UNDERLINED),
        (F_INVERSE, Modifier::REVERSED),
        (F_DIM, Modifier::DIM),
        (F_HIDDEN, Modifier::HIDDEN),
        (F_STRIKE, Modifier::CROSSED_OUT),
    ];
    for (flag, modifier) in mods {
        if flags & flag != 0 {
            style = style.add_modifier(modifier);
        }
    }
    if link {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

/// Draws the viewport into `inner`. Rows past the pane (a frame taller than a
/// pane that just shrank) are skipped.
pub fn draw(view: &TermView, buf: &mut Buffer, inner: Rect, options: &Options<'_>) -> Drawn {
    let mut drawn = Drawn::default();
    for (y, line) in view.lines.iter().enumerate().take(inner.height as usize) {
        let row_y = inner.y as i32 + y as i32;
        for run in line {
            let x0 = inner.x as i32 + run.col as i32;
            let x1 = put(buf, x0, row_y, &run.text, style(run.fg, run.bg, run.flags, run.link.is_some()), inner);
            if let Some(target) = &run.link
                && x1 > x0
                && x0 < inner.right() as i32
            {
                let x1 = x1.min(inner.right() as i32);
                drawn.links.push((Rect::new(x0 as u16, row_y as u16, (x1 - x0) as u16, 1), target.clone()));
            }
        }
    }
    if let Some((bg, bar)) = options.tint {
        tint_user_messages(buf, inner, &view.lines, bg, bar);
    }
    if let Some(selection) = options.selection {
        let ((_, r0), (_, r1)) = selection.ordered();
        for row in r0..=r1.min(inner.height.saturating_sub(1)) {
            for col in 0..inner.width {
                if selection.contains(col, row) {
                    buf[(inner.x + col, inner.y + row)].modifier.insert(Modifier::REVERSED);
                }
            }
        }
    }
    if options.cursor
        && let Some((cx, cy)) = view.cursor
        && cx < inner.width
        && cy < inner.height
    {
        drawn.cursor = Some((inner.x + cx, inner.y + cy));
    }
    drawn
}

/// Your own messages get their own background and a left bar.
fn tint_user_messages(buf: &mut Buffer, inner: Rect, lines: &[Vec<canvas_common::protocol::Run>], bg: Color, bar: Color) {
    let marked = user_message_rows(lines);
    for (row, _) in marked.iter().enumerate().take(inner.height as usize).filter(|(_, m)| **m) {
        let y = inner.y + row as u16;
        for col in 0..inner.width {
            if let Some(cell) = buf.cell_mut((inner.x + col, y))
                && cell.bg == Color::Reset
            {
                cell.set_bg(bg);
            }
        }
        if let Some(first) = buf.cell_mut((inner.x, y))
            && first.symbol() == " "
        {
            first.set_symbol("▎").set_fg(bar);
        }
    }
}

/// Writes text at signed coordinates, clipped to `clip`; zero-width
/// characters are dropped and wide ones blank their second cell. Returns the
/// end column.
fn put(buf: &mut Buffer, x: i32, y: i32, text: &str, style: Style, clip: Rect) -> i32 {
    let mut cx = x;
    let inside_row = y >= clip.y as i32 && y < clip.bottom() as i32;
    for c in text.chars().filter(|c| !c.is_control()) {
        let w = c.width().unwrap_or(0) as i32;
        if w == 0 {
            continue;
        }
        if inside_row && cx >= clip.x as i32 && cx + w <= clip.right() as i32 {
            if let Some(cell) = buf.cell_mut((cx as u16, y as u16)) {
                cell.set_char(c).set_style(style);
            }
            for extra in 1..w {
                if let Some(cell) = buf.cell_mut(((cx + extra) as u16, y as u16)) {
                    cell.reset();
                    cell.set_symbol("");
                }
            }
        }
        cx += w;
    }
    cx
}

#[cfg(test)]
mod tests {
    use super::*;
    use canvas_common::protocol::Run;

    #[test]
    fn user_message_tint_stays_inside_a_pane_that_shrank() {
        // A 20-row frame from before the pane shrank to 5 rows at the bottom of
        // the screen, with your message below the pane: used to index past the
        // buffer and panic.
        let run = |text: &str| Run { col: 0, text: text.into(), fg: 0, bg: 0, flags: 0, link: None };
        let mut lines = vec![vec![run(" ● output")]; 20];
        lines[2] = vec![run(" ❯ inside the pane")];
        lines[15] = vec![run(" ❯ below the pane")];
        let mut buf = Buffer::empty(Rect::new(0, 0, 30, 10));
        let inner = Rect::new(1, 4, 28, 5);
        tint_user_messages(&mut buf, inner, &lines, Color::Rgb(30, 44, 70), Color::Rgb(88, 166, 255));
        assert_eq!(buf[(1, 6)].symbol(), "▎", "visible message is marked");
        assert!((0..10).all(|y| y == 6 || buf[(1, y)].symbol() != "▎"), "nothing outside the pane");
    }

    #[test]
    fn osc8_runs_become_links_and_selection_reverses_cells() {
        let view = TermView {
            cols: 20,
            rows: 1,
            lines: vec![vec![Run { col: 2, text: "PR 1".into(), fg: 0, bg: 0, flags: 0, link: Some("https://x/pr/1".into()) }]],
            cursor: Some((3, 0)),
            ..Default::default()
        };
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 1));
        let selection = TermSelection { id: "t".into(), anchor: (0, 0), head: (1, 0) };
        let drawn = draw(&view, &mut buf, Rect::new(0, 0, 20, 1), &Options { tint: None, selection: Some(&selection), cursor: true });
        assert_eq!(drawn.links, vec![(Rect::new(2, 0, 4, 1), "https://x/pr/1".to_owned())]);
        assert_eq!(drawn.cursor, Some((3, 0)));
        assert!(buf[(1, 0)].modifier.contains(Modifier::REVERSED) && !buf[(2, 0)].modifier.contains(Modifier::REVERSED));
        assert!(buf[(2, 0)].modifier.contains(Modifier::UNDERLINED));
    }
}
