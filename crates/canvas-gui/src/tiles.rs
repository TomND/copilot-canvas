//! Terminal tiles: draws a `TermView` (protocol-8 rows: grapheme clusters,
//! engine widths, colours, underline styles, links) with its cursor,
//! selection, user-message tint and scroll position.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use canvas_common::protocol::{
    CURSOR_BAR, CURSOR_BLOCK_HOLLOW, CURSOR_UNDERLINE, F_BOLD, F_DIM, F_HIDDEN, F_INVERSE, F_ITALIC, F_STRIKE, F_UNDERLINE, F8_OVERLINE,
    F8_UL_MASK, F8_UL_SHIFT, Run, Run8, UL_CURLY, UL_DASHED, UL_DOTTED, UL_DOUBLE,
};
use canvas_core::app::TermSelection;
use canvas_core::view::TileModel;
use canvas_termview::TermView;
use unicode_width::UnicodeWidthChar;

use crate::quads::Quad;
use crate::scene::{self, Painter, cell_rect};
use crate::text::{Grid, Spec, in_mono_run};
use crate::theme::{self, BG, FG};

/// Underline shapes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Line {
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
    Strike,
    Over,
}

#[derive(Clone, Debug, PartialEq)]
struct Seg {
    col: u16,
    /// Cells covered (2 for a wide character).
    cells: u16,
    text: String,
    fg: [u8; 3],
    bold: bool,
    italic: bool,
    /// One character or cluster centred in its cells (not part of a run).
    alone: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct RowLayout {
    bgs: Vec<(u16, u16, [u8; 3])>,
    segs: Vec<Seg>,
    lines: Vec<(u16, u16, Line, [u8; 3])>,
    /// OSC 8 hyperlinks (underlined on hover or with Ctrl).
    links: Vec<(u16, u16, [u8; 3])>,
    /// Columns that hold a box-drawing or block character: (col, char, colour).
    boxes: Vec<(u16, char, [u8; 3])>,
}

struct Style {
    fg: [u8; 3],
    bg: Option<[u8; 3]>,
    bold: bool,
    italic: bool,
    hidden: bool,
}

fn style(fg: u32, bg: u32, flags: u32) -> Style {
    let mut fg = theme::protocol(fg, FG);
    let mut bg = (bg != 0).then(|| theme::protocol(bg, BG));
    if flags & F_INVERSE as u32 != 0 {
        let old = fg;
        fg = bg.unwrap_or(BG);
        bg = Some(old);
    }
    if flags & F_DIM as u32 != 0 {
        fg = theme::mix(fg, bg.unwrap_or(BG), 0.45);
    }
    Style { fg, bg, bold: flags & F_BOLD as u32 != 0, italic: flags & F_ITALIC as u32 != 0, hidden: flags & F_HIDDEN as u32 != 0 }
}

/// Adds the cells of one run (clusters at their columns) to a row layout.
fn add_cells<'a>(out: &mut RowLayout, cells: impl Iterator<Item = (u16, &'a str, u16)>, st: &Style) {
    for (col, cluster, width) in cells {
        if cluster == " " || cluster.is_empty() || st.hidden {
            continue;
        }
        let mut chars = cluster.chars();
        let first = chars.next().unwrap_or(' ');
        let single = chars.next().is_none();
        if single && width == 1 && crate::boxdraw::is_drawn(first) {
            out.boxes.push((col, first, st.fg));
            continue;
        }
        if single && width == 1 && in_mono_run(first) {
            if let Some(last) = out.segs.last_mut().filter(|s| !s.alone && s.col + s.cells == col && s.fg == st.fg && s.bold == st.bold && s.italic == st.italic) {
                last.text.push(first);
                last.cells += 1;
                continue;
            }
            out.segs.push(Seg { col, cells: 1, text: first.to_string(), fg: st.fg, bold: st.bold, italic: st.italic, alone: false });
            continue;
        }
        out.segs.push(Seg { col, cells: width.max(1), text: cluster.to_owned(), fg: st.fg, bold: st.bold, italic: st.italic, alone: true });
    }
}

fn layout8(row: &[Run8]) -> RowLayout {
    let mut out = RowLayout::default();
    for run in row {
        let st = style(run.fg, run.bg, run.flags);
        let width: u16 = if run.wide { 2 } else { 1 };
        let span = run.width();
        if let Some(bg) = st.bg {
            out.bgs.push((run.col, span, bg));
        }
        let cells = run.cells();
        add_cells(&mut out, cells.iter().enumerate().map(|(i, c)| (run.col + i as u16 * width, *c, width)), &st);
        let underline = theme::protocol(run.underline, st.fg);
        if run.flags & F_UNDERLINE as u32 != 0 {
            let kind = match (run.flags & F8_UL_MASK) >> F8_UL_SHIFT {
                UL_DOUBLE => Line::Double,
                UL_CURLY => Line::Curly,
                UL_DOTTED => Line::Dotted,
                UL_DASHED => Line::Dashed,
                _ => Line::Single,
            };
            out.lines.push((run.col, span, kind, underline));
        }
        if run.flags & F_STRIKE as u32 != 0 {
            out.lines.push((run.col, span, Line::Strike, st.fg));
        }
        if run.flags & F8_OVERLINE != 0 {
            out.lines.push((run.col, span, Line::Over, st.fg));
        }
        if run.link > 0 {
            out.links.push((run.col, span, underline));
        }
    }
    out
}

/// Protocol-7 rows (an older service): widths from the Unicode tables.
fn layout7(row: &[Run]) -> RowLayout {
    let mut out = RowLayout::default();
    for run in row {
        let st = style(run.fg, run.bg, run.flags as u32);
        let mut cells: Vec<(u16, String, u16)> = Vec::new();
        let mut col = run.col;
        for c in run.text.chars() {
            let w = c.width().unwrap_or(0) as u16;
            if w == 0 {
                if let Some(last) = cells.last_mut() {
                    last.1.push(c);
                }
                continue;
            }
            cells.push((col, c.to_string(), w));
            col += w;
        }
        let span = col - run.col;
        if let Some(bg) = st.bg {
            out.bgs.push((run.col, span, bg));
        }
        add_cells(&mut out, cells.iter().map(|(c, s, w)| (*c, s.as_str(), *w)), &st);
        if run.flags & F_UNDERLINE != 0 {
            out.lines.push((run.col, span, Line::Single, st.fg));
        }
        if run.flags & F_STRIKE != 0 {
            out.lines.push((run.col, span, Line::Strike, st.fg));
        }
        if run.link.is_some() {
            out.links.push((run.col, span, st.fg));
        }
    }
    out
}

fn hash_of<T: Hash>(value: &T) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut h);
    h.finish()
}

/// Row layouts per terminal, rebuilt only for rows whose content changed.
#[derive(Default)]
pub struct TileCache {
    rows: HashMap<String, Vec<(u64, RowLayout)>>,
    pub rebuilt: u64,
}

impl TileCache {
    fn rows(&mut self, id: &str, view: &TermView) -> &[(u64, RowLayout)] {
        let cached = self.rows.entry(id.to_owned()).or_default();
        let v8 = !view.rows8.is_empty();
        let count = if v8 { view.rows8.len() } else { view.lines.len() };
        cached.truncate(count);
        for y in 0..count {
            let hash = if v8 { hash_of(&view.rows8[y]) ^ 0x8 } else { hash_of(&view.lines[y]) };
            if cached.get(y).is_some_and(|(h, _)| *h == hash) {
                continue;
            }
            let layout = if v8 { layout8(&view.rows8[y]) } else { layout7(&view.lines[y]) };
            self.rebuilt += 1;
            if y < cached.len() {
                cached[y] = (hash, layout);
            } else {
                cached.push((hash, layout));
            }
        }
        cached
    }

    /// Forgets terminals that are gone.
    pub fn retain(&mut self, alive: impl Fn(&str) -> bool) {
        self.rows.retain(|id, _| alive(id));
    }
}

/// What the tile painter needs besides the views.
pub struct TileInput<'a> {
    pub selection: Option<&'a TermSelection>,
    /// Plain-text links found in terminals (screen rects in cells).
    pub links: &'a [canvas_core::links::Link],
    /// The pointer cell (for link hover) and whether Ctrl is held.
    pub hover: Option<(u16, u16)>,
    pub ctrl: bool,
    /// IME composition shown at the focused cursor.
    pub preedit: Option<&'a str>,
}

/// Where the focused terminal's cursor is (physical px), for the IME window.
pub type CursorRect = Option<[f32; 4]>;

pub fn paint(p: &mut Painter, grid: &Grid, tiles: &[TileModel], views: &HashMap<String, TermView>, cache: &mut TileCache, input: &TileInput) -> CursorRect {
    let (cw, ch) = (grid.cell_w, grid.cell_h);
    let layer = scene::TILES;
    let mut ime = None;
    for tile in tiles {
        let inner = tile.inner;
        let (x0, y0) = (inner.x as f32 * cw, inner.y as f32 * ch);
        let (x1, y1) = (inner.right() as f32 * cw, inner.bottom() as f32 * ch);
        let clip = [x0, y0, x1, y1];
        p.under(layer, cell_rect(x0, y0, x1, y1, theme::rgba(BG, 1.0), clip));
        let Some(view) = views.get(&tile.id) else { continue };
        let visible = (inner.height as usize).min(view.rows as usize);
        let tinted: Vec<bool> = if tile.tint { canvas_core::app::user_message_rows(&view.lines) } else { Vec::new() };
        let selection = input.selection.filter(|s| s.id == tile.id);
        let rows = cache.rows(&tile.id, view);
        let thin = grid.scale.round().max(1.0);
        for (y, (_, row)) in rows.iter().enumerate().take(visible) {
            let top = y0 + y as f32 * ch;
            let cell_x = |col: u16| x0 + col as f32 * cw;
            if tinted.get(y).copied().unwrap_or(false) {
                p.under(layer, cell_rect(x0, top, x1, top + ch, theme::color(canvas_core::paint::USER_BG, 1.0), clip));
            }
            for &(col, cells, bg) in &row.bgs {
                p.under(layer, cell_rect(cell_x(col), top, cell_x(col + cells), top + ch, theme::rgba(bg, 1.0), clip));
            }
            if let Some(sel) = selection {
                let ((c0, r0), (c1, r1)) = sel.ordered();
                let y = y as u16;
                if y >= r0 && y <= r1 {
                    let from = if y == r0 { c0 } else { 0 };
                    let to = if y == r1 { c1 + 1 } else { inner.width };
                    p.under(layer, cell_rect(cell_x(from), top, cell_x(to), top + ch, theme::color(canvas_core::paint::ACCENT, 0.38), clip));
                }
            }
            if tinted.get(y).copied().unwrap_or(false) && !row.segs.iter().any(|s| s.col == 0) && !row.boxes.iter().any(|b| b.0 == 0) {
                crate::boxdraw::draw('▎', x0, top, cw, ch, theme::color(canvas_core::paint::ACCENT, 1.0), clip, &mut p.scene.layers[layer].under);
            }
            for &(col, c, fg) in &row.boxes {
                crate::boxdraw::draw(c, cell_x(col), top, cw, ch, theme::rgba(fg, 1.0), clip, &mut p.scene.layers[layer].under);
            }
            for seg in &row.segs {
                let spec = Spec::mono(&seg.text, grid.font_px).bold(seg.bold).italic(seg.italic);
                if seg.alone {
                    let (key, width) = p.shape(spec);
                    let span = seg.cells as f32 * cw;
                    let left = cell_x(seg.col);
                    p.place(layer, key, left + ((span - width) / 2.0).max(-cw * 0.25), top, 1.0, seg.fg, clip);
                } else {
                    p.text(layer, spec, cell_x(seg.col), top, seg.fg, clip);
                }
            }
            for &(col, cells, kind, color) in &row.lines {
                line(p, kind, cell_x(col), cell_x(col + cells), top, ch, thin, theme::rgba(color, 1.0), clip);
            }
            let hovered = |col: u16, cells: u16| input.hover.is_some_and(|(hx, hy)| hy == inner.y + y as u16 && hx >= inner.x + col && hx < inner.x + col + cells);
            for &(col, cells, color) in &row.links {
                if input.ctrl || hovered(col, cells) {
                    line(p, Line::Single, cell_x(col), cell_x(col + cells), top, ch, thin, theme::rgba(color, 1.0), clip);
                }
            }
        }
        // Plain-text URLs and paths: underlined on hover or with Ctrl.
        for link in input.links.iter().filter(|l| l.terminal) {
            let hover = input.hover.is_some_and(|(x, y)| link.contains(x, y));
            if !(hover || input.ctrl) {
                continue;
            }
            for r in link.rects.iter().filter(|r| r.y >= inner.y && r.y < inner.bottom() && r.x >= inner.x && r.x < inner.right()) {
                let top = r.y as f32 * ch;
                line(p, Line::Single, r.x as f32 * cw, r.right() as f32 * cw, top, ch, thin, theme::color(canvas_core::paint::ACCENT, 1.0), clip);
            }
        }
        // Scrolled back: a thumb on the right edge shows where.
        if view.display_offset > 0 && view.history > 0 {
            let total = view.history as f32 + view.rows as f32;
            let height = ((view.rows as f32 / total) * (y1 - y0)).max(ch);
            let top = y0 + ((view.history - view.display_offset.min(view.history)) as f32 / total) * (y1 - y0);
            p.over(layer, Quad::rounded(x1 - 4.0 * thin, top, 3.0 * thin, height, 1.5 * thin, theme::color(canvas_core::paint::MUTED, 0.9), [0.0; 4], 0.0, clip));
        }
        if !tile.cursor {
            continue;
        }
        let Some((cx, cy)) = view.cursor.filter(|(cx, cy)| *cx < inner.width && *cy < inner.height) else { continue };
        let (left, top) = (x0 + cx as f32 * cw, y0 + cy as f32 * ch);
        ime = Some([left, top, cw, ch]);
        let cursor = theme::rgba([220, 226, 232], 1.0);
        if let Some(text) = input.preedit.filter(|t| !t.is_empty()) {
            let width = unicode_width::UnicodeWidthStr::width(text).max(1) as f32 * cw;
            let hud = scene::HUD;
            p.under(hud, cell_rect(left, top, left + width, top + ch, theme::rgba([45, 51, 80], 1.0), clip));
            p.text(hud, Spec::mono(text, grid.font_px), left, top, [255, 255, 255], clip);
            p.over(hud, Quad::rect(left.round(), (top + ch - 2.0 * thin).round(), width.round(), thin, theme::color(canvas_core::paint::ATTENTION, 1.0), clip));
            continue;
        }
        match view.cursor_shape {
            CURSOR_UNDERLINE => p.over(layer, Quad::rect(left.round(), (top + ch - 2.0 * thin).round(), cw, 2.0 * thin, cursor, clip)),
            CURSOR_BAR => p.over(layer, Quad::rect(left.round(), top.round(), 2.0 * thin, ch, cursor, clip)),
            CURSOR_BLOCK_HOLLOW => p.over(layer, Quad::rounded(left.round(), top.round(), cw, ch, 0.0, [0.0; 4], cursor, thin, clip)),
            _ => {
                p.under(layer, cell_rect(left, top, left + cw, top + ch, cursor, clip));
                // The character under a block cursor in the background colour.
                let row = rows.get(cy as usize).map(|(_, r)| r);
                if let Some(seg) = row.and_then(|r| r.segs.iter().find(|s| s.col <= cx && cx < s.col + s.cells)) {
                    let text: String = if seg.alone { seg.text.clone() } else { seg.text.chars().nth((cx - seg.col) as usize).map(String::from).unwrap_or_default() };
                    let left = if seg.alone { x0 + seg.col as f32 * cw } else { left };
                    let (key, width) = p.shape(Spec::mono(&text, grid.font_px).bold(seg.bold).italic(seg.italic));
                    let span = if seg.alone { seg.cells as f32 * cw } else { cw };
                    let x = if seg.alone { left + ((span - width) / 2.0).max(-cw * 0.25) } else { left };
                    p.place(layer, key, x, top, 1.0, BG, [left, top, left + span, top + ch]);
                }
            }
        }
    }
    ime
}

/// An underline, strikethrough or overline from `x0` to `x1` on the row at `top`.
#[allow(clippy::too_many_arguments)]
fn line(p: &mut Painter, kind: Line, x0: f32, x1: f32, top: f32, ch: f32, thin: f32, color: crate::quads::Rgba, clip: [f32; 4]) {
    let layer = scene::TILES;
    let base = (top + ch - thin - 1.0).round();
    let (x0, x1) = (x0.round(), x1.round());
    match kind {
        Line::Single => p.over(layer, Quad::rect(x0, base, x1 - x0, thin, color, clip)),
        Line::Double => {
            p.over(layer, Quad::rect(x0, base, x1 - x0, thin, color, clip));
            p.over(layer, Quad::rect(x0, base - 2.0 * thin, x1 - x0, thin, color, clip));
        }
        Line::Dotted => p.over(layer, Quad::line((x0 + thin, base + thin / 2.0), (x1, base + thin / 2.0), thin, color, 2.5 * thin, clip)),
        Line::Dashed => p.over(layer, Quad::line((x0 + thin, base + thin / 2.0), (x1, base + thin / 2.0), thin, color, 6.0 * thin, clip)),
        Line::Curly => {
            // A zig-zag of short segments, two waves per cell width.
            let amp = (1.5 * thin).max(1.0);
            let step = (ch / 6.0).max(2.0);
            let y = base - amp / 2.0;
            let mut x = x0;
            let mut up = true;
            while x < x1 {
                let nx = (x + step).min(x1);
                let (ya, yb) = if up { (y + amp, y - amp) } else { (y - amp, y + amp) };
                p.over(layer, Quad::line((x, ya), (nx, yb), thin, color, 0.0, clip));
                x = nx;
                up = !up;
            }
        }
        Line::Strike => p.over(layer, Quad::rect(x0, (top + ch / 2.0).round(), x1 - x0, thin, color, clip)),
        Line::Over => p.over(layer, Quad::rect(x0, top.round(), x1 - x0, thin, color, clip)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run8(col: u16, text: &str) -> Run8 {
        Run8 { col, text: text.into(), ..Default::default() }
    }

    #[test]
    fn rows_split_into_runs_wide_cells_and_box_drawing() {
        let mut wide = run8(4, "中文");
        wide.wide = true;
        let mut underlined = run8(9, "ul");
        underlined.flags = F_UNDERLINE as u32 | (UL_CURLY << F8_UL_SHIFT);
        underlined.underline = canvas_common::protocol::COLOR_RGB | 0xff0000;
        let row = layout8(&[run8(0, "ab c"), wide, run8(8, "│"), underlined]);
        let segs: Vec<(u16, u16, &str, bool)> = row.segs.iter().map(|s| (s.col, s.cells, s.text.as_str(), s.alone)).collect();
        assert_eq!(segs, [(0, 2, "ab", false), (3, 1, "c", false), (4, 2, "中", true), (6, 2, "文", true), (9, 2, "ul", false)]);
        assert_eq!(row.boxes, [(8, '│', FG)]);
        assert_eq!(row.lines, [(9, 2, Line::Curly, [255, 0, 0])]);
        // Grapheme clusters stay one cell.
        let mut flag = run8(0, "e\u{301}x");
        flag.clusters = vec![(0, 1)];
        let row = layout8(&[flag]);
        assert_eq!(row.segs.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(), ["e\u{301}", "x"]);
    }

    #[test]
    fn inverse_and_hidden_cells() {
        let mut inverse = run8(0, "x");
        inverse.flags = F_INVERSE as u32;
        let row = layout8(&[inverse]);
        assert_eq!(row.bgs, [(0, 1, FG)]);
        assert_eq!(row.segs[0].fg, BG);
        let mut hidden = run8(0, "secret");
        hidden.flags = F_HIDDEN as u32;
        assert!(layout8(&[hidden]).segs.is_empty());
        let v7 = layout7(&[Run { col: 2, text: "a中b".into(), fg: 0, bg: 0, flags: 0, link: Some("https://x".into()) }]);
        assert_eq!(v7.links, [(2, 4, FG)]);
        assert_eq!(v7.segs.iter().map(|s| s.col).collect::<Vec<_>>(), [2, 3, 5]);
    }
}
