//! The chrome (toolbar, activity bar, sidebar, panels, tile frames, status
//! bar, dialogs) as a grid of cells: backgrounds as quads, box drawing as
//! crisp lines, text in runs on the cell grid.

use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use unicode_width::UnicodeWidthStr;

use crate::quads::Quad;
use crate::scene::{self, Painter, cell_rect};
use crate::text::{Grid, Spec, in_mono_run};
use crate::theme::{self, BG, FG};

/// Where chrome cells go: the canvas and the terminal grids are drawn by
/// their own painters, so only non-blank chrome cells over them are drawn
/// (on top).
pub struct Regions<'a> {
    pub canvas: Option<Rect>,
    pub inners: &'a [Rect],
}

impl Regions<'_> {
    fn content(&self, x: u16, y: u16) -> bool {
        let inside = |r: &Rect| x >= r.x && x < r.right() && y >= r.y && y < r.bottom();
        self.canvas.as_ref().is_some_and(inside) || self.inners.iter().any(inside)
    }
}

fn blank(cell: &Cell) -> bool {
    cell.symbol() == " " && cell.bg == Color::Reset && !cell.modifier.contains(Modifier::REVERSED)
}

/// Resolved colours and font of a cell.
#[derive(Clone, Copy, PartialEq)]
struct Look {
    fg: [u8; 3],
    bg: Option<[u8; 3]>,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    hidden: bool,
}

fn look(cell: &Cell) -> Look {
    let m = cell.modifier;
    let mut fg = theme::rgb(cell.fg, FG);
    let mut bg = (cell.bg != Color::Reset).then(|| theme::rgb(cell.bg, BG));
    if m.contains(Modifier::REVERSED) {
        let old = fg;
        fg = bg.unwrap_or(BG);
        bg = Some(old);
    }
    if m.contains(Modifier::DIM) {
        fg = theme::mix(fg, bg.unwrap_or(BG), 0.45);
    }
    Look {
        fg,
        bg,
        bold: m.contains(Modifier::BOLD),
        italic: m.contains(Modifier::ITALIC),
        underline: m.contains(Modifier::UNDERLINED),
        strike: m.contains(Modifier::CROSSED_OUT),
        hidden: m.contains(Modifier::HIDDEN),
    }
}

/// A run of text being collected on one row.
struct Run {
    layer: usize,
    col: u16,
    text: String,
    look: Look,
    end: u16,
}

/// Paints the chrome buffer and the dialog overlay (cells not blank there).
pub fn paint(p: &mut Painter, grid: &Grid, chrome: &Buffer, overlay: Option<&Buffer>, regions: &Regions) {
    let area = chrome.area;
    let (cw, ch) = (grid.cell_w, grid.cell_h);
    let full = [0.0, 0.0, p.scene.width as f32, p.scene.height as f32];
    // Which buffer and layer each cell comes from.
    let source = |x: u16, y: u16| -> Option<(&Cell, usize)> {
        if let Some(cell) = overlay.and_then(|o| o.cell((x, y))).filter(|c| **c != Cell::EMPTY) {
            return Some((cell, scene::OVERLAY));
        }
        let cell = chrome.cell((x, y))?;
        if regions.content(x, y) {
            return (!blank(cell)).then_some((cell, scene::CHROME_TOP));
        }
        Some((cell, scene::BASE))
    };
    // Backgrounds, merged along each row.
    for y in area.y..area.bottom() {
        let mut span: Option<(usize, u16, [u8; 3])> = None;
        let mut last_bg: Option<[u8; 3]> = None;
        let top = y as f32 * ch;
        for x in area.x..=area.right() {
            let current = if x < area.right() {
                source(x, y).and_then(|(cell, layer)| {
                    let mut bg = look(cell).bg;
                    // The second half of a wide character keeps the first half's background.
                    if cell.symbol().is_empty() {
                        bg = bg.or(last_bg);
                    }
                    last_bg = bg;
                    match layer {
                        scene::BASE => Some((layer, bg.unwrap_or(BG))),
                        _ => bg.map(|bg| (layer, bg)),
                    }
                })
            } else {
                None
            };
            let continues = matches!((span, current), (Some((l, _, c)), Some((l2, c2))) if l == l2 && c == c2);
            if !continues {
                if let Some((layer, start, color)) = span.take() {
                    p.under(layer, cell_rect(start as f32 * cw, top, x as f32 * cw, top + ch, theme::rgba(color, 1.0), full));
                }
                span = current.map(|(layer, color)| (layer, x, color));
            }
        }
    }
    // Glyphs: box drawing as lines, text in runs.
    for y in area.y..area.bottom() {
        let top = y as f32 * ch;
        let mut run: Option<Run> = None;
        let flush = |p: &mut Painter, run: &mut Option<Run>| {
            if let Some(r) = run.take() {
                let text = r.text.trim_end();
                if !text.is_empty() && !r.look.hidden {
                    let spec = Spec::mono(text, grid.font_px).bold(r.look.bold).italic(r.look.italic);
                    p.text(r.layer, spec, r.col as f32 * cw, top, r.look.fg, full);
                }
            }
        };
        let mut x = area.x;
        while x < area.right() {
            let Some((cell, layer)) = source(x, y) else {
                flush(p, &mut run);
                x += 1;
                continue;
            };
            let symbol = cell.symbol();
            let cells = symbol.width().max(1) as u16;
            let lk = look(cell);
            let left = x as f32 * cw;
            if lk.underline || lk.strike {
                let color = theme::rgba(lk.fg, 1.0);
                let t = (grid.scale).round().max(1.0);
                let (w, yy) = (cells as f32 * cw, if lk.strike { top + (ch / 2.0).floor() } else { top + ch - t - 1.0 });
                p.over(layer, Quad::rect(left.round(), yy.round(), w.round(), t, color, full));
            }
            if symbol.is_empty() || symbol == " " {
                flush(p, &mut run);
                x += 1;
                continue;
            }
            let mut chars = symbol.chars();
            let first = chars.next().unwrap_or(' ');
            let single = chars.next().is_none();
            if single && !lk.hidden {
                let mut quads = Vec::new();
                if crate::boxdraw::draw(first, left, top, cw, ch, theme::rgba(lk.fg, 1.0), full, &mut quads) {
                    flush(p, &mut run);
                    p.scene.layers[layer].under.extend(quads);
                    x += 1;
                    continue;
                }
            }
            if single && cells == 1 && in_mono_run(first) {
                let joins = run.as_ref().is_some_and(|r| r.layer == layer && r.look == lk && r.end == x);
                if !joins {
                    flush(p, &mut run);
                    run = Some(Run { layer, col: x, text: String::new(), look: lk, end: x });
                }
                if let Some(r) = run.as_mut() {
                    r.text.push(first);
                    r.end = x + 1;
                }
                x += 1;
                continue;
            }
            // Symbols, emoji and wide characters: centred in their cells.
            flush(p, &mut run);
            if !lk.hidden {
                let (key, width) = p.shape(Spec::mono(symbol, grid.font_px).bold(lk.bold).italic(lk.italic));
                let span = cells as f32 * cw;
                let clip = [left - cw * 0.5, top, left + span + cw * 0.5, top + ch];
                p.place(layer, key, left + ((span - width) / 2.0).max(-cw * 0.25), top, 1.0, lk.fg, clip);
            }
            x += cells;
        }
        flush(p, &mut run);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::Scene;
    use ratatui::style::Style;

    #[test]
    fn regions_decide_where_cells_go() {
        let regions = Regions { canvas: Some(Rect::new(10, 1, 20, 10)), inners: &[Rect::new(40, 2, 10, 5)] };
        assert!(regions.content(10, 1) && regions.content(45, 6) && !regions.content(9, 1) && !regions.content(45, 7));
        let mut cell = Cell::EMPTY;
        assert!(blank(&cell));
        cell.set_style(Style::default().bg(Color::Rgb(1, 2, 3)));
        assert!(!blank(&cell));
        let mut dim = Cell::EMPTY;
        dim.set_style(Style::default().fg(Color::Rgb(200, 200, 200)).add_modifier(Modifier::DIM));
        assert_ne!(look(&dim).fg, [200, 200, 200], "dim text is blended towards the background");
        let _ = Scene::new(1, 1, BG);
    }
}
