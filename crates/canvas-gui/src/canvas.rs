//! The canvas, drawn from canvas-core's `CanvasModel` at any scale: groups,
//! connectors, status-styled boxes, loop arcs, pulses, the active terminal's
//! halo and the rubber band. Labels are shaped once and scaled with the zoom;
//! below a readable size boxes show one compact line, then shapes only.

use canvas_core::paint;
use canvas_core::view::{Border, BoxKind, CanvasModel, EdgeKind, WRect};
use ratatui::style::{Color, Modifier, Style};

use crate::quads::{Quad, Rgba};
use crate::scene::{self, Painter};
use crate::text::{Grid, Spec};
use crate::theme::{self, BG};

/// Smallest size for a box's full labels (logical px); below it a box shows
/// one compact line.
const MIN_LABEL: f32 = 9.0;
/// Smallest compact line (logical px).
const MIN_COMPACT: f32 = 6.5;

struct Ctx<'a> {
    model: &'a CanvasModel,
    grid: &'a Grid,
    clip: [f32; 4],
    /// DPI scale.
    dpi: f32,
}

impl Ctx<'_> {
    /// World → physical px.
    fn px(&self, wx: f32, wy: f32) -> (f32, f32) {
        let (x, y) = self.model.to_screen(wx, wy);
        (x * self.grid.cell_w, y * self.grid.cell_h)
    }

    /// A world rectangle in physical px, edges on whole pixels.
    fn rect(&self, r: WRect) -> [f32; 4] {
        let (x0, y0) = self.px(r.x, r.y);
        let (x1, y1) = self.px(r.right(), r.bottom());
        let (x0, y0, x1, y1) = (x0.round(), y0.round(), x1.round(), y1.round());
        [x0, y0, (x1 - x0).max(1.0), (y1 - y0).max(1.0)]
    }

    /// One world unit in px (horizontal, vertical).
    fn unit(&self) -> (f32, f32) {
        (self.model.scale * self.grid.cell_w, self.model.scale * self.grid.cell_h)
    }

    fn visible(&self, r: [f32; 4], margin: f32) -> bool {
        r[0] + r[2] + margin >= self.clip[0] && r[0] - margin <= self.clip[2] && r[1] + r[3] + margin >= self.clip[1] && r[1] - margin <= self.clip[3]
    }
}

/// A style's colour (dim blended towards the background).
fn style_color(style: Style, default: Color) -> [u8; 3] {
    let c = theme::rgb(style.fg.unwrap_or(default), theme::FG);
    if style.add_modifier.contains(Modifier::DIM) { theme::mix(c, BG, 0.5) } else { c }
}

fn rgb(c: Color) -> [u8; 3] {
    theme::rgb(c, theme::FG)
}

pub fn paint(p: &mut Painter, grid: &Grid, model: &CanvasModel) {
    let area = model.area;
    let clip = [area.x as f32 * grid.cell_w, area.y as f32 * grid.cell_h, area.right() as f32 * grid.cell_w, area.bottom() as f32 * grid.cell_h];
    let cx = Ctx { model, grid, clip, dpi: grid.scale };
    let layer = scene::CANVAS;
    p.under(layer, Quad::rect(clip[0], clip[1], clip[2] - clip[0], clip[3] - clip[1], theme::rgba(BG, 1.0), clip));
    let (ux, uy) = cx.unit();
    let label_px = grid.font_px * model.scale;
    let readable = label_px >= MIN_LABEL * cx.dpi;

    if model.grid {
        dots(p, &cx);
    }

    for group in &model.groups {
        let r = cx.rect(group.rect);
        if !cx.visible(r, 0.0) {
            continue;
        }
        let color = rgb(group.color);
        let strong = group.selected || group.drop_target;
        let radius = (0.6 * uy).clamp(3.0 * cx.dpi, 14.0 * cx.dpi);
        let (fill, border, width) = if strong { (0.09, 1.0, 2.0) } else { (0.045, 0.55, 1.0) };
        p.under(layer, Quad::rounded(r[0], r[1], r[2], r[3], radius, theme::rgba(color, fill), theme::rgba(color, border), width * cx.dpi, clip));
        // Title in the top row (the row that drags the group).
        let size = label_px.max(9.0 * cx.dpi).min(uy.max(9.0 * cx.dpi) * 0.9);
        let title = format!("{} {} · {}", group.icon, group.name, group.members);
        let left = r[0] + (0.9 * ux).max(6.0 * cx.dpi);
        let room = r[2] - (left - r[0]) - 6.0 * cx.dpi;
        if room > 20.0 * cx.dpi {
            let top = r[1] + ((uy.max(size * 1.3) - size * 1.3) / 2.0).max(2.0 * cx.dpi);
            p.text(layer, Spec::ui(&title, size).bold(true).fit(room), left, top, color, clip);
        }
        if model.scale >= canvas_core::ui_canvas::CHIP_SCALE {
            for chip in &group.chips {
                let c = cx.rect(chip.rect);
                let chip_color = if group.has_folder { color } else { rgb(paint::MUTED) };
                let pad = 0.15 * uy;
                p.under(scene::CANVAS_TOP, Quad::rounded(c[0], c[1] + pad, c[2], c[3] - 2.0 * pad, 4.0 * cx.dpi, theme::color(paint::PANEL, 1.0), theme::rgba(chip_color, 0.7), cx.dpi, clip));
                let (key, width) = p.shape(Spec::ui(chip.label.trim(), grid.font_px * 0.9));
                let scale = model.scale;
                let size = grid.font_px * 0.9 * scale;
                p.place(scene::CANVAS_TOP, key, c[0] + (c[2] - width * scale) / 2.0, c[1] + (c[3] - size * 1.3) / 2.0, scale, chip_color, clip);
            }
        }
        if readable && model.scale >= 0.6 {
            for (fx, fy, family) in &group.families {
                let (x, y) = cx.px(*fx, *fy);
                let width = r[0] + r[2] - x - ux;
                p.text(layer, Spec::ui(family, label_px * 0.9).italic(true).fit(width), x, y + (uy - label_px * 1.2) / 2.0, rgb(paint::MUTED), clip);
            }
        }
        if model.scale >= 0.35 {
            // The resize corner.
            let s = (0.9 * ux.min(uy)).clamp(5.0 * cx.dpi, 12.0 * cx.dpi);
            let (x1, y1) = (r[0] + r[2] - 3.0 * cx.dpi, r[1] + r[3] - 3.0 * cx.dpi);
            for i in 0..3 {
                let o = s * (i as f32 + 1.0) / 3.0;
                p.under(layer, Quad::line((x1 - o, y1), (x1, y1 - o), 1.2 * cx.dpi, theme::rgba(color, 0.8), 0.0, clip));
            }
        }
    }

    for edge in &model.edges {
        let points: Vec<(f32, f32)> = edge.path.iter().map(|&(x, y)| cx.px(x, y)).collect();
        let (dash, width) = match edge.kind {
            EdgeKind::Subsession => ((6.0 * cx.dpi * model.scale.clamp(0.5, 1.5)).max(3.0), 1.5 * cx.dpi),
            EdgeKind::Subagent => (0.0, 1.1 * cx.dpi),
        };
        let color = theme::rgba(rgb(edge.color), 0.85);
        for pair in points.windows(2) {
            p.under(layer, Quad::line(pair[0], pair[1], width, color, dash, clip));
        }
        if let Some(&end) = points.last() {
            let d = 2.2 * cx.dpi;
            p.under(layer, Quad::rounded(end.0 - d, end.1 - d, 2.0 * d, 2.0 * d, d, color, [0.0; 4], 0.0, clip));
        }
    }

    for b in &model.boxes {
        let r = cx.rect(b.rect);
        if !cx.visible(r, 4.0 * ux) {
            continue;
        }
        let border = style_color(b.border_style, paint::TEXT);
        let agent = b.kind == BoxKind::Agent;
        // Selected boxes (the ones being dragged) and their labels go above the rest.
        let layer = if b.selected { scene::CANVAS_TOP } else { scene::CANVAS };
        let inside = [r[0], r[1], r[0] + r[2], r[1] + r[3]];
        let radius = if agent { (0.25 * uy).clamp(2.0, 6.0 * cx.dpi) } else { (0.45 * uy).clamp(2.0, 10.0 * cx.dpi) };
        let fill = theme::rgba(rgb(b.fill), 1.0);
        let thin = cx.dpi.max(1.0);
        let edge = theme::rgba(border, 1.0);
        match b.border {
            Border::Heavy => p.under(layer, Quad::rounded(r[0], r[1], r[2], r[3], radius, fill, edge, 2.5 * thin, clip)),
            Border::Double => {
                p.under(layer, Quad::rounded(r[0], r[1], r[2], r[3], radius, fill, edge, thin, clip));
                let inset = 2.5 * thin;
                if r[2] > 4.0 * inset && r[3] > 4.0 * inset {
                    p.under(layer, Quad::rounded(r[0] + inset, r[1] + inset, r[2] - 2.0 * inset, r[3] - 2.0 * inset, (radius - inset).max(1.0), [0.0; 4], edge, thin, clip));
                }
            }
            Border::Dashed => p.under(layer, Quad::rounded(r[0], r[1], r[2], r[3], radius, fill, edge, 1.2 * thin, clip).dashed(7.0 * thin)),
            Border::Round | Border::Light => p.under(layer, Quad::rounded(r[0], r[1], r[2], r[3], radius, fill, edge, thin, clip)),
        }
        let pad_x = (0.9 * ux).max(3.0 * cx.dpi);
        let width = r[2] - 2.0 * pad_x;
        if readable {
            let lines = if b.rect.h >= 4.0 { 3 } else { 2 };
            let row = |i: usize| r[1] + (0.35 + i as f32) * uy;
            let at = |i: usize, dx: f32| (layer, r[0] + pad_x + dx, row(i), inside);
            label(p, &cx, &format!("{}{}", b.prefix, b.name), true, 1.0, at(0, 0.0), width, style_color(b.name_style, paint::TEXT));
            let used = label(p, &cx, &b.line, b.line_style.add_modifier.contains(Modifier::BOLD), 0.92, at(1, 0.0), width, style_color(b.line_style, paint::TEXT));
            if b.elsewhere && used < width - 20.0 * cx.dpi {
                label(p, &cx, " · ↗ elsewhere", false, 0.92, at(1, used), width - used, rgb(paint::PURPLE));
            }
            if lines == 3 {
                label(p, &cx, &b.subtitle, false, 0.85, at(2, 0.0), width, rgb(paint::MUTED));
            }
        } else if r[3] >= 9.0 * cx.dpi && r[2] >= 24.0 * cx.dpi {
            // One compact line, at a size the box can hold.
            let size = (r[3] * 0.62).min(grid.font_px * 0.85).max(MIN_COMPACT * cx.dpi);
            let k = ((size / grid.font_px) * 8.0).round().max(1.0) / 8.0;
            let text = format!("{} {}", b.icon, b.name);
            let (key, _) = p.shape(Spec::ui(&text, grid.font_px).bold(b.attention.is_some() || b.selected).fit((r[2] - 6.0 * cx.dpi) / k));
            let color = b.attention.map(|(c, _)| rgb(c)).unwrap_or_else(|| style_color(b.name_style, paint::TEXT));
            p.place(layer, key, r[0] + 3.0 * cx.dpi, r[1] + (r[3] - grid.font_px * k * 1.3) / 2.0, k, color, inside);
        } else if r[2] >= 6.0 && r[3] >= 4.0 {
            // Shapes only: a status dot.
            let d = (r[3] * 0.32).min(4.0 * cx.dpi).max(1.0);
            let color = b.attention.map(|(c, _)| rgb(c)).unwrap_or_else(|| style_color(b.line_style, paint::TEXT));
            p.under(layer, Quad::rounded(r[0] + d * 1.5, r[1] + r[3] / 2.0 - d, 2.0 * d, 2.0 * d, d, theme::rgba(color, 1.0), [0.0; 4], 0.0, clip));
        }
        if b.mailbox > 0 && model.scale >= 0.3 {
            badge(p, &cx, r, b.mailbox);
        }
    }

    for lp in &model.loops {
        let r = cx.rect(lp.rect);
        if !cx.visible(r, 4.0 * ux) {
            continue;
        }
        loop_arc(p, &cx, r, lp.halo, lp.phase);
    }

    if let Some((rect, typing)) = model.halo {
        let r = cx.rect(rect);
        let pad = halo_pad(&cx);
        let accent = rgb(paint::ACCENT);
        let radius = (0.45 * uy).clamp(2.0, 10.0 * cx.dpi) + pad;
        if typing {
            p.under(layer, Quad::rounded(r[0] - pad - 2.0 * cx.dpi, r[1] - pad - 2.0 * cx.dpi, r[2] + 2.0 * pad + 4.0 * cx.dpi, r[3] + 2.0 * pad + 4.0 * cx.dpi, radius + 2.0 * cx.dpi, [0.0; 4], theme::rgba(accent, 0.25), 3.0 * cx.dpi, clip));
        }
        let width = if typing { 2.5 } else { 1.5 } * cx.dpi;
        p.under(layer, Quad::rounded(r[0] - pad, r[1] - pad, r[2] + 2.0 * pad, r[3] + 2.0 * pad, radius, [0.0; 4], theme::rgba(accent, 1.0), width, clip));
    }

    for pulse in &model.pulses {
        let points: Vec<(f32, f32)> = pulse.path.iter().map(|&(x, y)| cx.px(x, y)).collect();
        if let Some(at) = along(&points, pulse.t) {
            glow_dot(p, scene::CANVAS_TOP, at, 3.0 * cx.dpi, rgb(paint::LOOP_LIT), clip);
        }
    }

    if let Some((x0, y0, x1, y1)) = model.rubber {
        let (x0, y0, x1, y1) = (x0 * grid.cell_w, y0 * grid.cell_h, x1 * grid.cell_w, y1 * grid.cell_h);
        let select = rgb(paint::SELECT);
        p.under(scene::CANVAS_TOP, Quad::rounded(x0, y0, (x1 - x0).max(1.0), (y1 - y0).max(1.0), 2.0 * cx.dpi, theme::rgba(select, 0.07), theme::rgba(select, 0.9), cx.dpi, clip).dashed(6.0 * cx.dpi));
    }

    if model.empty {
        let lines = canvas_core::ui_canvas::EMPTY_HINT;
        let size = grid.font_px * 1.05;
        let top = clip[1] + (clip[3] - clip[1]) / 2.0 - 3.0 * size * 1.5;
        for (i, line) in lines.iter().enumerate() {
            let (key, width) = p.shape(Spec::ui(line, size).bold(i == 0));
            let x = clip[0] + (clip[2] - clip[0] - width) / 2.0;
            p.place(layer, key, x, top + i as f32 * size * 1.5, 1.0, rgb(if i == 0 { paint::TEXT } else { paint::MUTED }), clip);
        }
    }
}

/// A box label shaped at the grid size (`rel` times) and drawn at the zoom;
/// returns its drawn width (px).
#[allow(clippy::too_many_arguments)]
fn label(p: &mut Painter, cx: &Ctx, text: &str, bold: bool, rel: f32, (layer, x, y, clip): (usize, f32, f32, [f32; 4]), width: f32, color: [u8; 3]) -> f32 {
    if text.is_empty() || width <= 0.0 {
        return 0.0;
    }
    let base = cx.grid.font_px * rel;
    let scale = cx.model.scale;
    let (key, w) = p.shape(Spec::ui(text, base).bold(bold).fit(width / scale));
    let clip = [clip[0].max(cx.clip[0]), clip[1].max(cx.clip[1]), clip[2].min(cx.clip[2]), clip[3].min(cx.clip[3])];
    p.place(layer, key, x, y + (cx.unit().1 - base * 1.3 * scale) / 2.0, scale, color, clip);
    w * scale
}

/// The halo's gap around the active box (px).
fn halo_pad(cx: &Ctx) -> f32 {
    (0.4 * cx.unit().0).clamp(3.0 * cx.dpi, 8.0 * cx.dpi)
}

fn dots(p: &mut Painter, cx: &Ctx) {
    let (ux, uy) = cx.unit();
    // Every 6 × 3 world units, thinned out by powers of two when zoomed out.
    let mut k = 1.0;
    while 6.0 * k * ux < 26.0 * cx.dpi || 3.0 * k * uy < 26.0 * cx.dpi {
        k *= 2.0;
    }
    let (sx, sy) = (6.0 * k, 3.0 * k);
    let v = cx.model.visible();
    let color = theme::color(paint::FAINT, 1.0);
    let d = (1.0 * cx.dpi).max(1.0);
    let mut wy = (v.y / sy).floor() * sy;
    while wy <= v.bottom() {
        let mut wx = (v.x / sx).floor() * sx;
        while wx <= v.right() {
            let (x, y) = cx.px(wx, wy);
            p.under(scene::CANVAS, Quad::rounded(x.round() - d, y.round() - d, 2.0 * d, 2.0 * d, d, color, [0.0; 4], 0.0, cx.clip));
            wx += sx;
        }
        wy += sy;
    }
}

fn badge(p: &mut Painter, cx: &Ctx, r: [f32; 4], count: u32) {
    let size = (cx.grid.font_px * cx.model.scale * 0.85).max(8.0 * cx.dpi);
    let text = format!("✉ {count}");
    let (key, width) = p.shape(Spec::ui(&text, size).bold(true));
    let (w, h) = (width + size * 0.9, size * 1.35);
    let (x, y) = (r[0] + r[2] - w - 1.5 * cx.unit().0, r[1] - h / 2.0);
    p.under(scene::CANVAS_TOP, Quad::rounded(x, y, w, h, h / 2.0, theme::color(paint::ATTENTION, 1.0), [0.0; 4], 0.0, cx.clip));
    p.place(scene::CANVAS_TOP, key, x + (w - width) / 2.0, y + (h - size * 1.3) / 2.0, 1.0, rgb(paint::PANEL), cx.clip);
}

/// The point `t` (0..1) of the way along a polyline.
fn along(points: &[(f32, f32)], t: f32) -> Option<(f32, f32)> {
    let lengths: Vec<f32> = points.windows(2).map(|w| ((w[1].0 - w[0].0).powi(2) + (w[1].1 - w[0].1).powi(2)).sqrt()).collect();
    let total: f32 = lengths.iter().sum();
    if total <= 0.0 {
        return points.first().copied();
    }
    let mut left = t.clamp(0.0, 1.0) * total;
    for (pair, len) in points.windows(2).zip(&lengths) {
        if left <= *len && *len > 0.0 {
            let f = left / len;
            return Some((pair[0].0 + (pair[1].0 - pair[0].0) * f, pair[0].1 + (pair[1].1 - pair[0].1) * f));
        }
        left -= len;
    }
    points.last().copied()
}

fn glow_dot(p: &mut Painter, layer: usize, (x, y): (f32, f32), r: f32, color: [u8; 3], clip: [f32; 4]) {
    let g = r * 2.2;
    p.under(layer, Quad::rounded(x - g, y - g, 2.0 * g, 2.0 * g, g, theme::rgba(color, 0.22), [0.0; 4], 0.0, clip));
    p.under(layer, Quad::rounded(x - r, y - r, 2.0 * r, 2.0 * r, r, theme::rgba(color, 1.0), [0.0; 4], 0.0, clip));
}

/// The loopback on a looping box's right side: out along the top row, down,
/// and back into the bottom row, with a lit dot running around it.
fn loop_arc(p: &mut Painter, cx: &Ctx, r: [f32; 4], halo: bool, phase: f32) {
    let (ux, uy) = cx.unit();
    let color = rgb(paint::LOOP);
    let lit = rgb(paint::LOOP_LIT);
    let start = r[0] + r[2] + if halo { halo_pad(cx) + 2.0 * cx.dpi } else { 0.0 };
    if r[3] < 12.0 * cx.dpi {
        // Too small for an arc: a pulsing ↻.
        let size = (r[3] * 0.9).max(8.0 * cx.dpi);
        let on = ((phase * 4.0) as u32).is_multiple_of(2);
        p.text(scene::CANVAS, Spec::ui("↻", size).bold(true), start + 2.0 * cx.dpi, r[1] + (r[3] - size * 1.3) / 2.0, if on { lit } else { color }, cx.clip);
        return;
    }
    let (top, bottom) = (r[1] + 0.5 * uy, r[1] + r[3] - 0.5 * uy);
    let right = start + (2.2 * ux).max(8.0 * cx.dpi);
    let rr = ((bottom - top) / 2.0).min(1.2 * ux).max(2.0);
    let mut points = vec![(start, top), (right - rr, top)];
    for i in 1..=6 {
        let a = std::f32::consts::FRAC_PI_2 * (1.0 - i as f32 / 6.0);
        points.push((right - rr + rr * a.cos(), top + rr - rr * a.sin()));
    }
    points.push((right, bottom - rr));
    for i in 1..=6 {
        let a = std::f32::consts::FRAC_PI_2 * (i as f32 / 6.0);
        points.push((right - rr + rr * a.cos(), bottom - rr + rr * a.sin()));
    }
    points.push((start, bottom));
    let width = 1.3 * cx.dpi;
    let line: Rgba = theme::rgba(color, 0.9);
    for pair in points.windows(2) {
        p.under(scene::CANVAS, Quad::line(pair[0], pair[1], width, line, 0.0, cx.clip));
    }
    // Arrow head back into the box.
    let a = 4.0 * cx.dpi;
    p.under(scene::CANVAS, Quad::line((start, bottom), (start + a, bottom - a * 0.8), width, line, 0.0, cx.clip));
    p.under(scene::CANVAS, Quad::line((start, bottom), (start + a, bottom + a * 0.8), width, line, 0.0, cx.clip));
    for (offset, size) in [(0.06, 0.55), (0.03, 0.75), (0.0, 1.0)] {
        if let Some(at) = along(&points, (phase - offset).rem_euclid(1.0)) {
            if size < 1.0 {
                let d = 2.0 * cx.dpi * size;
                p.under(scene::CANVAS, Quad::rounded(at.0 - d, at.1 - d, 2.0 * d, 2.0 * d, d, theme::rgba(color, 1.0), [0.0; 4], 0.0, cx.clip));
            } else {
                glow_dot(p, scene::CANVAS, at, 2.6 * cx.dpi, lit, cx.clip);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_along_a_polyline() {
        let path = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)];
        assert_eq!(along(&path, 0.0), Some((0.0, 0.0)));
        assert_eq!(along(&path, 0.5), Some((10.0, 0.0)));
        assert_eq!(along(&path, 0.75), Some((10.0, 5.0)));
        assert_eq!(along(&path, 1.0), Some((10.0, 10.0)));
        assert_eq!(style_color(Style::default().fg(Color::Rgb(200, 0, 0)), Color::Reset), [200, 0, 0]);
    }
}
