//! Low-level drawing helpers, box styles and the color theme.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthChar;

pub struct BoxSet {
    pub tl: &'static str,
    pub tr: &'static str,
    pub bl: &'static str,
    pub br: &'static str,
    pub h: &'static str,
    pub v: &'static str,
}

pub const ROUND: BoxSet = BoxSet { tl: "╭", tr: "╮", bl: "╰", br: "╯", h: "─", v: "│" };
pub const HEAVY: BoxSet = BoxSet { tl: "┏", tr: "┓", bl: "┗", br: "┛", h: "━", v: "┃" };
pub const LIGHT: BoxSet = BoxSet { tl: "┌", tr: "┐", bl: "└", br: "┘", h: "─", v: "│" };
pub const DASHED: BoxSet = BoxSet { tl: "┌", tr: "┐", bl: "└", br: "┘", h: "╌", v: "╎" };
pub const DOUBLE: BoxSet = BoxSet { tl: "╔", tr: "╗", bl: "╚", br: "╝", h: "═", v: "║" };

// Palette: safe for red/green color blindness. States are told apart on the
// blue-yellow axis and by lightness, and always also by icon or border shape;
// red vs green is never the only difference.
pub const ACCENT: Color = Color::Rgb(88, 166, 255);
/// Selection is bright white so it never looks like a status colour.
pub const SELECT: Color = Color::Rgb(240, 246, 252);
pub const MUTED: Color = Color::Rgb(110, 118, 129);
pub const FAINT: Color = Color::Rgb(56, 62, 72);
pub const TEXT: Color = Color::Rgb(201, 209, 217);
pub const PANEL: Color = Color::Rgb(22, 27, 34);
pub const PANEL_HI: Color = Color::Rgb(33, 38, 45);
pub const MODAL: Color = Color::Rgb(28, 33, 40);
/// OK / enabled / success (sky blue, not green).
pub const POSITIVE: Color = Color::Rgb(86, 180, 233);
/// Needs you (gold).
pub const ATTENTION: Color = Color::Rgb(255, 176, 0);
/// Just finished (bright blue).
pub const FINISHED: Color = Color::Rgb(100, 143, 255);
pub const BLUE: Color = Color::Rgb(88, 166, 255);
pub const GRAY: Color = Color::Rgb(139, 148, 158);
pub const DARK: Color = Color::Rgb(88, 94, 102);
/// Failure (vermillion: darker than gold, never paired with green).
pub const NEGATIVE: Color = Color::Rgb(230, 97, 0);
pub const PURPLE: Color = Color::Rgb(163, 113, 247);
/// Looping sessions (Okabe-Ito bluish green; distinct from blue, gold and purple).
pub const LOOP: Color = Color::Rgb(0, 158, 115);
pub const LOOP_LIT: Color = Color::Rgb(110, 240, 190);
/// Background of your own messages in Copilot terminals.
pub const USER_BG: Color = Color::Rgb(30, 44, 70);
/// Background tints for compact zoom levels.
pub const ATTENTION_BG: Color = Color::Rgb(66, 48, 6);
pub const FINISHED_BG: Color = Color::Rgb(26, 36, 78);
pub const NEGATIVE_BG: Color = Color::Rgb(62, 26, 4);
pub const NEGATIVE_DIM: Color = Color::Rgb(128, 54, 0);

/// Okabe-Ito colours (designed to stay distinct with colour blindness).
pub const GROUP_COLORS: [Color; 6] = [
    Color::Rgb(86, 180, 233),
    Color::Rgb(230, 159, 0),
    Color::Rgb(32, 190, 150),
    Color::Rgb(204, 121, 167),
    Color::Rgb(240, 228, 66),
    Color::Rgb(230, 110, 40),
];

pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn fg(color: Color) -> Style {
    Style::default().fg(color)
}

pub fn bold(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

pub fn text_width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Truncates to `width` display cells, ending with `…` when shortened.
pub fn truncate(text: &str, width: usize) -> String {
    if text_width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// Keeps the end of a long path visible: `…\parent\leaf`.
pub fn truncate_left(text: &str, width: usize) -> String {
    if text_width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut kept: Vec<char> = Vec::new();
    let mut used = 0;
    for c in text.chars().rev() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        kept.push(c);
        used += w;
    }
    kept.reverse();
    format!("…{}", kept.into_iter().collect::<String>())
}

fn sanitize(c: char) -> Option<char> {
    if c.is_control() { None } else { Some(c) }
}

/// Writes text at signed coordinates, clipped to `clip`. Returns the end column.
pub fn put(buf: &mut Buffer, x: i32, y: i32, text: &str, style: Style, clip: Rect) -> i32 {
    let mut cx = x;
    let inside_row = y >= clip.y as i32 && y < clip.bottom() as i32;
    for c in text.chars().filter_map(sanitize) {
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

pub fn put_sym(buf: &mut Buffer, x: i32, y: i32, symbol: &str, style: Style, clip: Rect) {
    if x >= clip.x as i32 && x < clip.right() as i32 && y >= clip.y as i32 && y < clip.bottom() as i32
        && let Some(cell) = buf.cell_mut((x as u16, y as u16))
    {
        cell.set_symbol(symbol).set_style(style);
    }
}

pub fn style_at(buf: &mut Buffer, x: i32, y: i32, style: Style, clip: Rect) {
    if x >= clip.x as i32 && x < clip.right() as i32 && y >= clip.y as i32 && y < clip.bottom() as i32
        && let Some(cell) = buf.cell_mut((x as u16, y as u16))
    {
        cell.set_style(style);
    }
}

pub fn fill(buf: &mut Buffer, x: i32, y: i32, w: i32, h: i32, style: Style, clip: Rect) {
    let x0 = x.max(clip.x as i32);
    let y0 = y.max(clip.y as i32);
    let x1 = (x + w).min(clip.right() as i32);
    let y1 = (y + h).min(clip.bottom() as i32);
    for yy in y0..y1 {
        for xx in x0..x1 {
            if let Some(cell) = buf.cell_mut((xx as u16, yy as u16)) {
                cell.reset();
                cell.set_style(style);
            }
        }
    }
}

/// Draws a box outline; interior untouched.
#[allow(clippy::too_many_arguments)]
pub fn outline(buf: &mut Buffer, x: i32, y: i32, w: i32, h: i32, set: &BoxSet, style: Style, clip: Rect) {
    if w < 2 || h < 2 {
        return;
    }
    let right = x + w - 1;
    let bottom = y + h - 1;
    // Only iterate the visible span so huge off-screen groups stay cheap.
    let hx0 = (x + 1).max(clip.x as i32 - 1);
    let hx1 = right.min(clip.right() as i32 + 1);
    for xx in hx0..hx1 {
        put_sym(buf, xx, y, set.h, style, clip);
        put_sym(buf, xx, bottom, set.h, style, clip);
    }
    let vy0 = (y + 1).max(clip.y as i32 - 1);
    let vy1 = bottom.min(clip.bottom() as i32 + 1);
    for yy in vy0..vy1 {
        put_sym(buf, x, yy, set.v, style, clip);
        put_sym(buf, right, yy, set.v, style, clip);
    }
    put_sym(buf, x, y, set.tl, style, clip);
    put_sym(buf, right, y, set.tr, style, clip);
    put_sym(buf, x, bottom, set.bl, style, clip);
    put_sym(buf, right, bottom, set.br, style, clip);
}

pub fn rect_contains(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_display_width() {
        assert_eq!(truncate("hello world", 8), "hello w…");
        assert_eq!(truncate("中文中文", 5), "中文…");
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate_left(r"C:\very\long\path\leaf", 10), "…path\\leaf");
    }

    #[test]
    fn put_clips_and_handles_wide_characters() {
        let area = Rect::new(0, 0, 6, 2);
        let mut buf = Buffer::empty(area);
        let clip = Rect::new(1, 0, 4, 1);
        let end = put(&mut buf, -1, 0, "ab中cd", Style::default(), clip);
        assert_eq!(end, 5);
        assert_eq!(buf[(1, 0)].symbol(), "中");
        assert_eq!(buf[(3, 0)].symbol(), "c");
        assert_eq!(buf[(0, 0)].symbol(), " ", "outside the clip is untouched");
        put(&mut buf, 0, 1, "zzz", Style::default(), clip);
        assert_eq!(buf[(0, 1)].symbol(), " ", "rows outside the clip are untouched");
    }
}
