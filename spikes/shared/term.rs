//! Terminal-tile helpers shared by both spikes: protocol colours (xterm
//! palette), run styles, splitting a row into cell-aligned text segments, and
//! the tile layout.

#![allow(dead_code)]

use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

use canvas_common::protocol::{COLOR_DEFAULT, COLOR_RGB, F_BOLD, F_DIM, F_HIDDEN, F_INVERSE, F_ITALIC, F_STRIKE, F_UNDERLINE, Run};

use crate::scene::Rect;

pub const DEFAULT_FG: u32 = 0xd4d4d4;
pub const DEFAULT_BG: u32 = 0x121317;
pub const COLS: u16 = 200;
pub const ROWS: u16 = 60;

/// The standard xterm 256-colour palette as 0xRRGGBB.
pub fn palette() -> &'static [u32; 256] {
    static PALETTE: OnceLock<[u32; 256]> = OnceLock::new();
    PALETTE.get_or_init(|| {
        let mut p = [0u32; 256];
        let base = [
            0x000000, 0xcd0000, 0x00cd00, 0xcdcd00, 0x0000ee, 0xcd00cd, 0x00cdcd, 0xe5e5e5, 0x7f7f7f, 0xff0000, 0x00ff00, 0xffff00, 0x5c5cff,
            0xff00ff, 0x00ffff, 0xffffff,
        ];
        p[..16].copy_from_slice(&base);
        let level = |v: u32| if v == 0 { 0 } else { 55 + v * 40 };
        for i in 0..216u32 {
            let (r, g, b) = (i / 36, (i / 6) % 6, i % 6);
            p[16 + i as usize] = (level(r) << 16) | (level(g) << 8) | level(b);
        }
        for i in 0..24u32 {
            let v = 8 + i * 10;
            p[232 + i as usize] = (v << 16) | (v << 8) | v;
        }
        p
    })
}

/// Protocol colour → 0xRRGGBB: 0 default, `COLOR_RGB` bit, else palette index + 1.
pub fn resolve(color: u32, default: u32) -> u32 {
    if color == COLOR_DEFAULT {
        default
    } else if color & COLOR_RGB != 0 {
        color & 0xff_ffff
    } else {
        palette()[((color - 1) & 0xff) as usize]
    }
}

fn mix(a: u32, b: u32, t: f32) -> u32 {
    let ch = |s: u32| -> (f32, f32) { (((a >> s) & 0xff) as f32, ((b >> s) & 0xff) as f32) };
    let mut out = 0;
    for s in [16, 8, 0] {
        let (x, y) = ch(s);
        out |= ((x * t + y * (1.0 - t)).round() as u32) << s;
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Style {
    pub fg: u32,
    pub bg: Option<u32>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
}

pub fn style_of(run: &Run) -> Style {
    let mut fg = resolve(run.fg, DEFAULT_FG);
    let mut bg = (run.bg != COLOR_DEFAULT).then(|| resolve(run.bg, DEFAULT_BG));
    if run.flags & F_INVERSE != 0 {
        let old = fg;
        fg = bg.unwrap_or(DEFAULT_BG);
        bg = Some(old);
    }
    if run.flags & F_DIM != 0 {
        fg = mix(fg, bg.unwrap_or(DEFAULT_BG), 0.6);
    }
    if run.flags & F_HIDDEN != 0 {
        fg = bg.unwrap_or(DEFAULT_BG);
    }
    Style {
        fg,
        bg,
        bold: run.flags & F_BOLD != 0,
        italic: run.flags & F_ITALIC != 0,
        underline: run.flags & F_UNDERLINE != 0,
        strike: run.flags & F_STRIKE != 0,
    }
}

/// Text placed at a column. ASCII text is shaped as one segment (the primary
/// monospace font's advance is the cell width); every other character gets its
/// own segment at its own column so fallback fonts and wide characters (CJK,
/// emoji: two cells) never shift the grid.
#[derive(Clone, Debug, PartialEq)]
pub struct Seg {
    pub col: u16,
    pub cells: u16,
    pub text: String,
    pub style: Style,
    /// Hash of text + font style (shaping cache key).
    pub hash: u64,
}

/// A coloured span (backgrounds) or decoration (underline/strike) in cells.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Span {
    pub col: u16,
    pub cells: u16,
    pub color: u32,
}

#[derive(Default, Clone, Debug)]
pub struct RowLayout {
    pub segs: Vec<Seg>,
    pub bgs: Vec<Span>,
    pub underlines: Vec<Span>,
    pub strikes: Vec<Span>,
    pub cells: usize,
}

fn char_width(c: char) -> usize {
    unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
}

pub fn layout_row(runs: &[Run]) -> RowLayout {
    let mut out = RowLayout::default();
    for run in runs {
        let style = style_of(run);
        let mut col = run.col as usize;
        let start = col;
        let mut ascii = String::new();
        let mut ascii_col = col;
        let flush = |ascii: &mut String, ascii_col: usize, out: &mut RowLayout| {
            let lead = ascii.len() - ascii.trim_start_matches(' ').len();
            let text = ascii.trim_matches(' ');
            if !text.is_empty() {
                let cells = text.chars().filter(|c| char_width(*c) > 0).count() as u16;
                out.segs.push(Seg { col: (ascii_col + lead) as u16, cells, text: text.to_owned(), style, hash: 0 });
            }
            ascii.clear();
        };
        for ch in run.text.chars() {
            if (' '..='~').contains(&ch) {
                if ascii.is_empty() {
                    ascii_col = col;
                }
                ascii.push(ch);
                col += 1;
                continue;
            }
            let width = char_width(ch);
            if width == 0 {
                // Combining mark: joins the previous segment's cluster.
                if ascii.is_empty() {
                    if let Some(last) = out.segs.last_mut() {
                        last.text.push(ch);
                    }
                } else {
                    ascii.push(ch);
                }
                continue;
            }
            flush(&mut ascii, ascii_col, &mut out);
            out.segs.push(Seg { col: col as u16, cells: width as u16, text: ch.to_string(), style, hash: 0 });
            col += width;
        }
        flush(&mut ascii, ascii_col, &mut out);
        let cells = (col - start) as u16;
        if let Some(bg) = style.bg {
            out.bgs.push(Span { col: start as u16, cells, color: bg });
        }
        if style.underline {
            out.underlines.push(Span { col: start as u16, cells, color: style.fg });
        }
        if style.strike {
            out.strikes.push(Span { col: start as u16, cells, color: style.fg });
        }
        out.cells += cells as usize;
    }
    for seg in &mut out.segs {
        seg.hash = text_hash(&seg.text, &seg.style);
    }
    out
}

pub fn row_hash(runs: &[Run]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    runs.hash(&mut h);
    h.finish()
}

pub fn text_hash(text: &str, style: &Style) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    style.bold.hash(&mut h);
    style.italic.hash(&mut h);
    h.finish()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// Canvas on the left half, four tiles 2×2 on the right half (each shows the
    /// top-left part of its 200×60 grid at the readable font size).
    Normal,
    /// Four tiles 2×2 over the whole window, font fitted so every one of the
    /// 4 × 200 × 60 cells is drawn (the "tiles only" stress layout).
    TilesOnly,
}

pub const TILE_FONT_PX: f32 = 12.0;
pub const TILE_HEADER_PX: f32 = 20.0;
pub const LINE_HEIGHT: f32 = 1.2;
const GAP: f32 = 6.0;

#[derive(Clone, Copy, Debug)]
pub struct TileGeom {
    pub outer: Rect,
    /// The cell grid's area (below the header).
    pub grid: Rect,
    pub font_px: f32,
    pub cell_w: f32,
    pub cell_h: f32,
}

impl TileGeom {
    pub fn visible_cols(&self) -> u16 {
        ((self.grid.w / self.cell_w + 0.001).floor() as u16).min(COLS)
    }
    pub fn visible_rows(&self) -> u16 {
        ((self.grid.h / self.cell_h + 0.001).floor() as u16).min(ROWS)
    }
}

/// First grid row shown when only `visible` of `rows` fit: keeps the cursor
/// (where output appears) on screen.
pub fn first_row(rows: usize, visible: usize, cursor: Option<(u16, u16)>) -> usize {
    let bottom = cursor.map(|(_, y)| y as usize + 1).unwrap_or(rows).max(visible).min(rows);
    bottom.saturating_sub(visible)
}

/// Canvas rectangle and four tile geometries for a window (logical px).
/// `advance` is the monospace font's advance per px of font size.
pub fn layout(window: Rect, mode: Layout, advance: f32) -> (Rect, [TileGeom; 4]) {
    let (canvas, area) = match mode {
        Layout::Normal => {
            let half = (window.w / 2.0).floor();
            (Rect::new(window.x, window.y, half, window.h), Rect::new(window.x + half, window.y, window.w - half, window.h))
        }
        Layout::TilesOnly => (Rect::new(window.x, window.y, 0.0, 0.0), window),
    };
    let tw = ((area.w - 3.0 * GAP) / 2.0).max(1.0);
    let th = ((area.h - 3.0 * GAP) / 2.0).max(1.0);
    let geoms = std::array::from_fn(|i| {
        let outer = Rect::new(area.x + GAP + (i % 2) as f32 * (tw + GAP), area.y + GAP + (i / 2) as f32 * (th + GAP), tw, th);
        let grid = Rect::new(outer.x + 4.0, outer.y + TILE_HEADER_PX, (outer.w - 8.0).max(1.0), (outer.h - TILE_HEADER_PX - 4.0).max(1.0));
        let font_px = match mode {
            Layout::Normal => TILE_FONT_PX,
            Layout::TilesOnly => (grid.w / (COLS as f32 * advance)).min(grid.h / (ROWS as f32 * LINE_HEIGHT)).clamp(3.0, 24.0),
        };
        let cell_w = font_px * advance;
        let cell_h = match mode {
            Layout::Normal => (font_px * LINE_HEIGHT).round(),
            Layout::TilesOnly => font_px * LINE_HEIGHT,
        };
        TileGeom { outer, grid, font_px, cell_w, cell_h }
    });
    (canvas, geoms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(col: u16, text: &str, fg: u32, flags: u16) -> Run {
        Run { col, text: text.into(), fg, bg: 0, flags, link: None }
    }

    #[test]
    fn palette_and_rgb_colours_resolve() {
        assert_eq!(resolve(0, 0x123456), 0x123456);
        assert_eq!(resolve(COLOR_RGB | 0xabcdef, 0), 0xabcdef);
        assert_eq!(resolve(2, 0), 0xcd0000, "index 1 (red) is sent as 2");
        assert_eq!(resolve(17, 0), 0x000000, "index 16 is the cube's black");
        assert_eq!(resolve(256, 0), 0xeeeeee);
    }

    #[test]
    fn wide_and_symbol_characters_get_their_own_columns() {
        let row = layout_row(&[run(0, "ab 中文 ✓x", 0, 0)]);
        let cols: Vec<(u16, u16, &str)> = row.segs.iter().map(|s| (s.col, s.cells, s.text.as_str())).collect();
        assert_eq!(cols, [(0, 2, "ab"), (3, 2, "中"), (5, 2, "文"), (8, 1, "✓"), (9, 1, "x")]);
        assert_eq!(row.cells, 10);
    }

    #[test]
    fn inverse_swaps_and_spaces_are_trimmed() {
        let row = layout_row(&[run(4, "  hi  ", 2, F_INVERSE | F_UNDERLINE)]);
        assert_eq!(row.segs[0].col, 6);
        assert_eq!(row.segs[0].style.fg, DEFAULT_BG);
        assert_eq!(row.bgs, [Span { col: 4, cells: 6, color: 0xcd0000 }]);
        assert_eq!(row.underlines.len(), 1);
    }

    #[test]
    fn tiles_only_fits_the_whole_grid() {
        let (_, tiles) = layout(Rect::new(0.0, 0.0, 1800.0, 1000.0), Layout::TilesOnly, 0.586);
        for t in tiles {
            assert_eq!((t.visible_cols(), t.visible_rows()), (COLS, ROWS));
        }
    }
}
