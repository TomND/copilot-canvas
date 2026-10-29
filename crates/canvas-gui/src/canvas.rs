//! The canvas, drawn from canvas-core's `CanvasModel` at any scale: groups,
//! connectors, status-styled boxes, loop arcs, pulses, the active terminal's
//! halo and the rubber band. Labels are shaped once and scaled with the zoom;
//! below a readable size boxes show one compact line, then shapes only.
//! Loop arcs have a layer of their own ([`paint_loops`]): while their dots
//! move, a frame repaints only that layer.

use canvas_core::loops;
use canvas_core::paint;
use canvas_core::view::{Border, BoxKind, CanvasModel, EdgeKind, LoopModel, Motion, WRect};
use ratatui::style::{Color, Modifier, Style};

use crate::quads::Quad;
use crate::scene::{self, Painter};
use crate::text::{Grid, Spec};
use crate::theme::{self, BG};

/// Smallest size for a box's full labels (logical px); below it a box shows
/// one compact line.
const MIN_LABEL: f32 = 9.0;
/// Smallest compact line (logical px).
const MIN_COMPACT: f32 = 6.5;

pub(crate) struct Ctx<'a> {
    pub(crate) model: &'a CanvasModel,
    pub(crate) grid: &'a Grid,
    pub(crate) clip: [f32; 4],
    /// DPI scale.
    pub(crate) dpi: f32,
}

impl Ctx<'_> {
    /// World → physical px.
    pub(crate) fn px(&self, wx: f32, wy: f32) -> (f32, f32) {
        let (x, y) = self.model.to_screen(wx, wy);
        (x * self.grid.cell_w, y * self.grid.cell_h)
    }

    /// A world rectangle in physical px, edges on whole pixels.
    pub(crate) fn rect(&self, r: WRect) -> [f32; 4] {
        let (x0, y0) = self.px(r.x, r.y);
        let (x1, y1) = self.px(r.right(), r.bottom());
        let (x0, y0, x1, y1) = (x0.round(), y0.round(), x1.round(), y1.round());
        [x0, y0, (x1 - x0).max(1.0), (y1 - y0).max(1.0)]
    }

    /// One world unit in px (horizontal, vertical).
    pub(crate) fn unit(&self) -> (f32, f32) {
        (self.model.scale * self.grid.cell_w, self.model.scale * self.grid.cell_h)
    }

    pub(crate) fn visible(&self, r: [f32; 4], margin: f32) -> bool {
        r[0] + r[2] + margin >= self.clip[0] && r[0] - margin <= self.clip[2] && r[1] + r[3] + margin >= self.clip[1] && r[1] - margin <= self.clip[3]
    }
}

/// A style's colour (dim blended towards the background).
fn style_color(style: Style, default: Color) -> [u8; 3] {
    let c = theme::rgb(style.fg.unwrap_or(default), theme::FG);
    if style.add_modifier.contains(Modifier::DIM) { theme::mix(c, BG, 0.5) } else { c }
}

pub(crate) fn rgb(c: Color) -> [u8; 3] {
    theme::rgb(c, theme::FG)
}

/// The canvas of a frame; animations (loop dots) at `clock_ms`.
pub fn paint(p: &mut Painter, grid: &Grid, model: &CanvasModel, clock_ms: u64) {
    let area = model.area;
    let clip = [area.x as f32 * grid.cell_w, area.y as f32 * grid.cell_h, area.right() as f32 * grid.cell_w, area.bottom() as f32 * grid.cell_h];
    let cx = Ctx { model, grid, clip, dpi: grid.scale };
    let layer = scene::CANVAS;
    p.under(layer, Quad::rect(clip[0], clip[1], clip[2] - clip[0], clip[3] - clip[1], theme::rgba(BG, 1.0), clip));
    let (ux, uy) = cx.unit();
    let label_px = grid.font_px * model.scale;
    let readable = label_px >= MIN_LABEL * cx.dpi;

    // The dot grid stays while panning (cheap here; the TUI hides it).
    dots(p, &cx);

    for group in &model.groups {
        let r = cx.rect(group.rect);
        if !cx.visible(r, 0.0) {
            continue;
        }
        let color = rgb(group.color);
        let radius = (0.6 * uy).clamp(3.0 * cx.dpi, 14.0 * cx.dpi);
        // A selected group is unmistakable: a heavier outline and a stronger tint.
        let (fill, border, width) = if group.selected { (0.12, 1.0, 3.0) } else if group.drop_target { (0.09, 1.0, 2.0) } else { (0.045, 0.55, 1.0) };
        p.under(layer, Quad::rounded(r[0], r[1], r[2], r[3], radius, theme::rgba(color, fill), theme::rgba(color, border), width * cx.dpi, clip));
        // Title in the top row (the row that drags the group), up to its chips.
        let size = label_px.max(9.0 * cx.dpi).min(uy.max(9.0 * cx.dpi) * 0.9);
        let title = format!("{} {} · {}", group.icon, group.name, group.members);
        let left = r[0] + (0.9 * ux).max(6.0 * cx.dpi);
        let chips_left = group.chips.first().map(|c| cx.rect(c.rect)[0]).unwrap_or(r[0] + r[2]);
        let room = chips_left.min(r[0] + r[2]) - left - 6.0 * cx.dpi;
        if room > 20.0 * cx.dpi {
            let top = r[1] + ((uy.max(size * 1.3) - size * 1.3) / 2.0).max(2.0 * cx.dpi);
            p.text(layer, Spec::ui(&title, size).bold(true).fit(room), left, top, color, clip);
        }
        for chip in &group.chips {
            let c = cx.rect(chip.rect);
            let chip_color = if chip.lit { color } else { rgb(paint::MUTED) };
            let pad = if chip.fixed { 0.1 * grid.cell_h } else { 0.15 * uy };
            p.under(scene::CANVAS_TOP, Quad::rounded(c[0], c[1] + pad, c[2], c[3] - 2.0 * pad, 4.0 * cx.dpi, theme::color(paint::PANEL, 1.0), theme::rgba(chip_color, 0.7), cx.dpi, clip));
            // World chips scale with the zoom; zoomed out they keep a readable size.
            let scale = if chip.fixed { 0.85 } else { model.scale };
            let (key, width) = p.shape(Spec::ui(chip.label.trim(), grid.font_px * 0.9));
            let size = grid.font_px * 0.9 * scale;
            p.place(scene::CANVAS_TOP, key, c[0] + (c[2] - width * scale) / 2.0, c[1] + (c[3] - size * 1.3) / 2.0, scale, chip_color, clip);
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

    let levels = box_levels(&model.boxes);
    let top_boxes = scene::BOXES + levels.iter().copied().max().unwrap_or(0);
    for (b, level) in model.boxes.iter().zip(&levels) {
        let r = cx.rect(b.rect);
        if !cx.visible(r, 4.0 * ux) || b.alpha <= 0.01 {
            continue;
        }
        // Boxes flying into a stack (or out of it) fade with their alpha.
        let alpha = b.alpha.clamp(0.0, 1.0);
        let fade = |c: [u8; 3]| if alpha >= 1.0 { c } else { theme::mix(BG, c, alpha) };
        let border = fade(style_color(b.border_style, paint::TEXT));
        let agent = b.kind == BoxKind::Agent;
        // A box's fill and labels share a layer, above the boxes it overlaps;
        // selected boxes (the ones being dragged) go above them all.
        let layer = if b.selected { scene::CANVAS_TOP } else { scene::BOXES + level };
        let inside = [r[0].max(clip[0]), r[1].max(clip[1]), (r[0] + r[2]).min(clip[2]), (r[1] + r[3]).min(clip[3])];
        let radius = if agent { (0.25 * uy).clamp(2.0, 6.0 * cx.dpi) } else { (0.45 * uy).clamp(2.0, 10.0 * cx.dpi) };
        let fill = theme::rgba(rgb(b.fill), alpha);
        let thin = cx.dpi.max(1.0);
        let edge = theme::rgba(border, alpha);
        if let Some(deck) = &b.deck {
            // The cards behind the top one peek out right and below.
            for i in (1..=deck.back).rev() {
                let o = i as f32 * (0.55 * ux).clamp(3.0 * cx.dpi, 9.0 * cx.dpi);
                p.under(layer, Quad::rounded(r[0] + o, r[1] + o * 0.9, r[2], r[3], radius, theme::rgba(theme::mix(rgb(paint::PANEL), BG, 0.25 * i as f32), alpha), theme::rgba(rgb(paint::MUTED), 0.8 * alpha), thin, clip));
            }
        }
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
        if b.dust > 0 {
            dust(p, &cx, layer, r, b.dust, &b.id, inside);
        }
        let pad_x = (0.9 * ux).max(3.0 * cx.dpi);
        let width = r[2] - 2.0 * pad_x;
        if readable {
            let lines = if b.rect.h >= 4.0 { 3 } else { 2 };
            let row = |i: usize| r[1] + (0.35 + i as f32) * uy;
            let at = |i: usize, dx: f32| (layer, r[0] + pad_x + dx, row(i), inside);
            label(p, &cx, &format!("{}{}", b.prefix, b.name), true, 1.0, at(0, 0.0), width, fade(style_color(b.name_style, paint::TEXT)));
            let used = label(p, &cx, &b.line, b.line_style.add_modifier.contains(Modifier::BOLD), 0.92, at(1, 0.0), width, fade(style_color(b.line_style, paint::TEXT)));
            if b.elsewhere && used < width - 20.0 * cx.dpi {
                label(p, &cx, " · ↗ elsewhere", false, 0.92, at(1, used), width - used, fade(rgb(paint::PURPLE)));
            }
            if lines == 3 {
                label(p, &cx, &b.subtitle, false, 0.85, at(2, 0.0), width, fade(rgb(paint::MUTED)));
            }
        } else if r[3] >= 9.0 * cx.dpi && r[2] >= 24.0 * cx.dpi {
            // One compact line, at a size the box can hold.
            let size = (r[3] * 0.62).min(grid.font_px * 0.85).max(MIN_COMPACT * cx.dpi);
            let k = ((size / grid.font_px) * 8.0).round().max(1.0) / 8.0;
            let text = format!("{} {}", b.icon, b.name);
            let (key, _) = p.shape(Spec::ui(&text, grid.font_px).bold(b.attention.is_some() || b.selected).fit((r[2] - 6.0 * cx.dpi) / k));
            let color = b.attention.map(|(c, _)| rgb(c)).unwrap_or_else(|| style_color(b.name_style, paint::TEXT));
            p.place(layer, key, r[0] + 3.0 * cx.dpi, r[1] + (r[3] - grid.font_px * k * 1.3) / 2.0, k, fade(color), inside);
        } else if r[2] >= 6.0 && r[3] >= 4.0 {
            // Shapes only: a status dot.
            let d = (r[3] * 0.32).min(4.0 * cx.dpi).max(1.0);
            let color = b.attention.map(|(c, _)| rgb(c)).unwrap_or_else(|| style_color(b.line_style, paint::TEXT));
            p.under(layer, Quad::rounded(r[0] + d * 1.5, r[1] + r[3] / 2.0 - d, 2.0 * d, 2.0 * d, d, theme::rgba(color, alpha), [0.0; 4], 0.0, clip));
        }
        if b.mailbox > 0 && model.scale >= 0.3 && alpha >= 1.0 {
            badge(p, &cx, r, b.mailbox);
        }
        if model.scale >= 0.2 && alpha >= 1.0 {
            if let Some(deck) = b.deck.as_ref().filter(|d| d.count > 1) {
                pill(p, &cx, (r[0] + r[2] - 1.5 * ux, r[1]), true, &format!("+{}", deck.count - 1), rgb(paint::ACCENT));
            }
            if let Some((count, color)) = b.hidden {
                // Neutral unless a hidden one needs attention (then its colour).
                let (bg, fg) = match color {
                    Some(color) => (rgb(color), rgb(paint::PANEL)),
                    None => (rgb(paint::PANEL_HI), rgb(paint::TEXT)),
                };
                pill_with(p, &cx, (r[0] + r[2] - 1.5 * ux, r[1] + r[3]), true, &format!("⋯ {count} hidden"), bg, fg);
            }
        }
    }

    for effect in &model.effects {
        crate::effects::paint(p, &cx, effect, model.motion);
    }

    loops(p, &cx, clock_ms);

    if let Some((rect, typing)) = model.halo {
        let layer = top_boxes;
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

/// A pill-shaped badge centred on a box edge point; `right`: it ends there.
pub(crate) fn pill(p: &mut Painter, cx: &Ctx, (x, y): (f32, f32), right: bool, text: &str, bg: [u8; 3]) {
    pill_with(p, cx, (x, y), right, text, bg, rgb(paint::PANEL));
}

/// A pill with its own text colour.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pill_with(p: &mut Painter, cx: &Ctx, (x, y): (f32, f32), right: bool, text: &str, bg: [u8; 3], fg: [u8; 3]) {
    let size = (cx.grid.font_px * cx.model.scale * 0.8).clamp(8.0 * cx.dpi, cx.grid.font_px * 0.9);
    let (key, width) = p.shape(Spec::ui(text, size).bold(true));
    let (w, h) = (width + size * 0.9, size * 1.35);
    let left = if right { x - w } else { x };
    p.under(scene::CANVAS_TOP, Quad::rounded(left, y - h / 2.0, w, h, h / 2.0, theme::rgba(bg, 1.0), [0.0; 4], 0.0, cx.clip));
    p.place(scene::CANVAS_TOP, key, left + (w - width) / 2.0, y - h / 2.0 + (h - size * 1.3) / 2.0, 1.0, fg, cx.clip);
}

/// A small, stable hash of a box id (dust and effects place their specks with it).
pub(crate) fn seed(id: &str) -> u32 {
    id.bytes().fold(2_166_136_261u32, |h, b| (h ^ b as u32).wrapping_mul(16_777_619))
}

/// A pseudo-random number in 0..1 from a seed and an index.
pub(crate) fn noise(seed: u32, i: u32) -> f32 {
    let mut x = seed ^ i.wrapping_mul(0x9e37_79b9);
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^= x >> 16;
    (x & 0xff_ffff) as f32 / 0x100_0000 as f32
}

/// Dust on an unused session's box (static): specks over its body and a
/// cobweb in the top-right corner, both growing with the level.
fn dust(p: &mut Painter, cx: &Ctx, layer: usize, r: [f32; 4], level: u8, id: &str, clip: [f32; 4]) {
    let (ux, uy) = cx.unit();
    let level = level.clamp(1, 3) as usize;
    let s = seed(id);
    let speck = theme::rgba([150, 146, 136], [0.28, 0.36, 0.46][level - 1]);
    let d = (0.08 * uy).clamp(0.7 * cx.dpi, 1.6 * cx.dpi);
    for i in 0..[7u32, 16, 30][level - 1] {
        let (x, y) = (r[0] + 2.0 * d + noise(s, i) * (r[2] - 4.0 * d), r[1] + 2.0 * d + noise(s, i + 997) * (r[3] - 4.0 * d));
        let size = d * (0.6 + noise(s, i + 31) * 0.8);
        p.under(layer, Quad::rounded(x - size, y - size, 2.0 * size, 2.0 * size, size, speck, [0.0; 4], 0.0, clip));
    }
    // The cobweb: threads from the corner and a few arcs across them.
    let web = theme::rgba([182, 178, 168], [0.30, 0.38, 0.48][level - 1]);
    let len = [1.1f32, 1.9, 2.8][level - 1] * ux.min(uy * 2.0).max(4.0 * cx.dpi);
    let corner = (r[0] + r[2] - 1.5 * cx.dpi, r[1] + 1.5 * cx.dpi);
    let width = (0.6 * cx.dpi).max(0.8);
    let angles = [0.0f32, 0.42, 0.85, 1.25, std::f32::consts::FRAC_PI_2];
    let point = |a: f32, l: f32| (corner.0 - l * a.cos(), corner.1 + l * a.sin() * 0.75);
    for a in angles {
        p.under(layer, Quad::line(corner, point(a, len), width, web, 0.0, clip));
    }
    for ring in 1..=level + 1 {
        let l = len * ring as f32 / (level + 1) as f32;
        for pair in angles.windows(2) {
            // A thread that sags a little between two spokes.
            let (a, b) = (point(pair[0], l), point(pair[1], l));
            let mid = point((pair[0] + pair[1]) / 2.0, l * 0.9);
            p.under(layer, Quad::line(a, mid, width, web, 0.0, clip));
            p.under(layer, Quad::line(mid, b, width, web, 0.0, clip));
        }
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

pub(crate) fn glow_dot(p: &mut Painter, layer: usize, (x, y): (f32, f32), r: f32, color: [u8; 3], clip: [f32; 4]) {
    glow(p, layer, (x, y), r, color, 1.0, clip);
}

/// A dot with a soft halo, at `alpha`.
fn glow(p: &mut Painter, layer: usize, (x, y): (f32, f32), r: f32, color: [u8; 3], alpha: f32, clip: [f32; 4]) {
    let g = r * 2.2;
    p.under(layer, Quad::rounded(x - g, y - g, 2.0 * g, 2.0 * g, g, theme::rgba(color, 0.22 * alpha), [0.0; 4], 0.0, clip));
    p.under(layer, Quad::rounded(x - r, y - r, 2.0 * r, 2.0 * r, r, theme::rgba(color, alpha), [0.0; 4], 0.0, clip));
}

/// Segments per quarter circle of a loop's bends.
const BEND: usize = 12;
/// The comet's tail: samples, and how far back in the lap it reaches.
const TAIL: usize = 16;
const TAIL_LAP: f32 = 0.3;

/// A polyline measured along its length.
pub(crate) struct Path {
    pub(crate) points: Vec<(f32, f32)>,
    /// Length up to each point.
    lengths: Vec<f32>,
}

impl Path {
    fn new(points: Vec<(f32, f32)>) -> Self {
        let mut lengths = Vec::with_capacity(points.len());
        let mut total = 0.0;
        for (i, p) in points.iter().enumerate() {
            if i > 0 {
                let q = points[i - 1];
                total += ((p.0 - q.0).powi(2) + (p.1 - q.1).powi(2)).sqrt();
            }
            lengths.push(total);
        }
        Self { points, lengths }
    }

    fn total(&self) -> f32 {
        self.lengths.last().copied().unwrap_or(0.0)
    }

    /// The point `s` (0..1) of the way along, at sub-pixel precision.
    pub(crate) fn at(&self, s: f32) -> (f32, f32) {
        let want = s.clamp(0.0, 1.0) * self.total();
        let i = self.lengths.partition_point(|l| *l < want).clamp(1, self.points.len().saturating_sub(1).max(1));
        let (a, b) = (self.points[i - 1], self.points[i.min(self.points.len() - 1)]);
        let span = self.lengths[i.min(self.lengths.len() - 1)] - self.lengths[i - 1];
        let f = if span > 0.0 { (want - self.lengths[i - 1]) / span } else { 0.0 };
        (a.0 + (b.0 - a.0) * f, a.1 + (b.1 - a.1) * f)
    }

    /// The polyline from its start to `s` of the way along.
    fn upto(&self, s: f32) -> Vec<(f32, f32)> {
        let want = s.clamp(0.0, 1.0) * self.total();
        let mut out: Vec<(f32, f32)> = self.points.iter().zip(&self.lengths).take_while(|(_, l)| **l < want).map(|(p, _)| *p).collect();
        out.push(self.at(s));
        out
    }
}

/// Where a loop's arc starts (px): the box's right edge, or outside the halo.
fn loop_start(cx: &Ctx, r: [f32; 4], halo: bool) -> f32 {
    r[0] + r[2] + if halo { halo_pad(cx) + 2.0 * cx.dpi } else { 0.0 }
}

/// The loopback beside a box (px): out along the top row, round the bend,
/// down, and back into the bottom row; `None` when the box is too small for
/// an arc (it gets a ↻ instead).
pub(crate) fn loop_path(cx: &Ctx, r: [f32; 4], halo: bool) -> Option<Path> {
    if r[3] < 12.0 * cx.dpi {
        return None;
    }
    let (ux, uy) = cx.unit();
    let start = loop_start(cx, r, halo);
    let (top, bottom) = (r[1] + 0.5 * uy, r[1] + r[3] - 0.5 * uy);
    let right = start + (2.2 * ux).max(8.0 * cx.dpi);
    let rr = ((bottom - top) / 2.0).min(1.2 * ux).max(2.0);
    let mut points = vec![(start, top), (right - rr, top)];
    for i in 1..=BEND {
        let a = std::f32::consts::FRAC_PI_2 * (1.0 - i as f32 / BEND as f32);
        points.push((right - rr + rr * a.cos(), top + rr - rr * a.sin()));
    }
    points.push((right, bottom - rr));
    for i in 1..=BEND {
        let a = std::f32::consts::FRAC_PI_2 * (i as f32 / BEND as f32);
        points.push((right - rr + rr * a.cos(), bottom - rr + rr * a.sin()));
    }
    points.push((start, bottom));
    Some(Path::new(points))
}

/// The loop arcs of a canvas model at `clock_ms`, into the loop layer (a
/// frame while they move repaints only this).
pub fn paint_loops(p: &mut Painter, grid: &Grid, model: &CanvasModel, clock_ms: u64) {
    let area = model.area;
    let clip = [area.x as f32 * grid.cell_w, area.y as f32 * grid.cell_h, area.right() as f32 * grid.cell_w, area.bottom() as f32 * grid.cell_h];
    loops(p, &Ctx { model, grid, clip, dpi: grid.scale }, clock_ms);
}

fn loops(p: &mut Painter, cx: &Ctx, clock_ms: u64) {
    let (ux, _) = cx.unit();
    for lp in &cx.model.loops {
        let r = cx.rect(lp.rect);
        if cx.visible(r, 4.0 * ux) {
            loop_arc(p, cx, r, lp, clock_ms);
        }
    }
}

/// Where a loop's lit dot is at `clock_ms` (px; `None`: no arc, or no
/// moving dot with reduced motion).
pub(crate) fn loop_dot(cx: &Ctx, lp: &LoopModel, clock_ms: u64) -> Option<(f32, f32)> {
    let path = loop_path(cx, cx.rect(lp.rect), lp.halo)?;
    (cx.model.motion == Motion::On).then(|| path.at(loops::along(lp.lap_at(clock_ms))))
}

/// How often the loop layer needs a frame: so that the fastest dot in view
/// moves about 2 px (at 100 % DPI) a frame. That is about 30 frames/s for
/// arcs at 100 % zoom and fewer for smaller ones (zoomed out: down to 20).
pub fn loop_interval(grid: &Grid, model: &CanvasModel) -> std::time::Duration {
    let area = model.area;
    let clip = [area.x as f32 * grid.cell_w, area.y as f32 * grid.cell_h, area.right() as f32 * grid.cell_w, area.bottom() as f32 * grid.cell_h];
    let cx = Ctx { model, grid, clip, dpi: grid.scale };
    let longest = model
        .loops
        .iter()
        .filter_map(|lp| {
            let r = cx.rect(lp.rect);
            cx.visible(r, 4.0 * cx.unit().0).then(|| loop_path(&cx, r, lp.halo).map(|p| p.total())).flatten()
        })
        .fold(0.0f32, f32::max);
    // The dot runs one arc length a lap, a third faster at its fastest.
    let speed = longest * 1.35 / (loops::LAP_MS as f32 / 1000.0);
    let ms = if speed > 0.0 { 2.0 * cx.dpi / speed * 1000.0 } else { 50.0 };
    std::time::Duration::from_millis(ms.clamp(33.0, 50.0) as u64)
}

/// The loop arcs of a model at `clock_ms` as the test hooks report them:
/// box id, lap, grow, ending, whether the dot moves, and the dot (physical
/// px and fractional cells) with the arc's bounds (px).
pub fn loop_info(grid: &Grid, model: &CanvasModel, clock_ms: u64) -> Vec<serde_json::Value> {
    let area = model.area;
    let clip = [area.x as f32 * grid.cell_w, area.y as f32 * grid.cell_h, area.right() as f32 * grid.cell_w, area.bottom() as f32 * grid.cell_h];
    let cx = Ctx { model, grid, clip, dpi: grid.scale };
    model
        .loops
        .iter()
        .map(|lp| {
            let r = cx.rect(lp.rect);
            let bounds = loop_path(&cx, r, lp.halo).map(|path| {
                let (xs, ys): (Vec<f32>, Vec<f32>) = path.points.iter().copied().unzip();
                let min = |v: &[f32]| v.iter().copied().fold(f32::MAX, f32::min);
                let max = |v: &[f32]| v.iter().copied().fold(f32::MIN, f32::max);
                [min(&xs), min(&ys), max(&xs), max(&ys)]
            });
            let dot = loop_dot(&cx, lp, clock_ms);
            serde_json::json!({
                "id": lp.id,
                "lap": lp.lap_at(clock_ms),
                "grow": lp.grow_at(clock_ms),
                "ending": lp.ending,
                "moving": model.loops_move(),
                "dot_px": dot.map(|(x, y)| [x, y]),
                "dot": dot.map(|(x, y)| [x / grid.cell_w, y / grid.cell_h]),
                "bounds_px": bounds,
            })
        })
        .collect()
}

/// One loopback arc: the track (growing in after the loop started, fading
/// after it stopped), the arrowhead back into the box, and with motion on a
/// comet: a glowing head where the clock puts it, a tapering tail and a soft
/// pulse at the arrowhead after each lap. With reduced motion: the arc, its
/// arrowhead and a ↻ badge, still. Small boxes get a ↻ that breathes.
fn loop_arc(p: &mut Painter, cx: &Ctx, r: [f32; 4], lp: &LoopModel, clock_ms: u64) {
    let layer = scene::LOOPS;
    let grow = lp.grow_at(clock_ms);
    if grow <= 0.002 {
        return;
    }
    let (color, lit) = (rgb(paint::LOOP), rgb(paint::LOOP_LIT));
    let moving = cx.model.motion == Motion::On;
    let fade = if lp.ending { grow } else { 1.0 };
    let t = lp.lap_at(clock_ms);
    let d = cx.dpi;
    let Some(path) = loop_path(cx, r, lp.halo) else {
        let size = (r[3] * 0.9).max(8.0 * d);
        let (x, y) = (loop_start(cx, r, lp.halo) + 2.0 * d, r[1] + (r[3] - size * 1.3) / 2.0);
        let breath = if moving { 0.5 + 0.5 * (4.0 * std::f32::consts::PI * t).cos() } else { 1.0 };
        if moving {
            let (gx, gy, rad) = (x + size * 0.45, r[1] + r[3] / 2.0, size * (0.5 + 0.15 * breath));
            p.under(layer, Quad::rounded(gx - rad, gy - rad, 2.0 * rad, 2.0 * rad, rad, theme::rgba(lit, (0.08 + 0.2 * breath) * fade * grow), [0.0; 4], 0.0, cx.clip));
        }
        let glyph = theme::mix(BG, theme::mix(color, lit, breath), fade * grow);
        p.text(layer, Spec::ui("↻", size).bold(true), x, y, glyph, cx.clip);
        return;
    };
    let width = 1.3 * d;
    // The track: dimmer while a comet runs on it.
    let track = theme::rgba(color, if moving { 0.6 } else { 0.95 } * fade);
    let shown = if lp.ending { 1.0 } else { grow };
    for pair in path.upto(shown).windows(2) {
        p.under(layer, Quad::line(pair[0], pair[1], width, track, 0.0, cx.clip));
    }
    if !lp.ending && grow < 1.0 {
        // The tip drawing the arc as it grows in.
        glow(p, layer, path.at(grow), 2.0 * d, lit, 0.9, cx.clip);
    }
    // The arrowhead back into the box, once the arc is whole.
    let arrow = if lp.ending { fade } else { ((grow - 0.85) / 0.15).clamp(0.0, 1.0) };
    let end = *path.points.last().expect("an arc has points");
    if arrow > 0.0 {
        let a = 4.0 * d;
        let head = theme::rgba(if moving { color } else { lit }, 0.95 * arrow);
        p.under(layer, Quad::line(end, (end.0 + a, end.1 - a * 0.8), width, head, 0.0, cx.clip));
        p.under(layer, Quad::line(end, (end.0 + a, end.1 + a * 0.8), width, head, 0.0, cx.clip));
    }
    if !moving {
        // Reduced motion: a still ↻ on the far side says "repeats".
        if arrow > 0.0 {
            let (mx, my) = path.at(0.5);
            let size = (10.0 * d).min((r[3] * 0.5).max(8.0 * d));
            let rad = size * 0.72;
            p.under(layer, Quad::rounded(mx - rad, my - rad, 2.0 * rad, 2.0 * rad, rad, theme::color(paint::PANEL, arrow), theme::rgba(color, 0.95 * arrow), width, cx.clip));
            let (key, w) = p.shape(Spec::ui("↻", size).bold(true));
            p.place(layer, key, mx - w / 2.0, my - size * 0.74, 1.0, theme::mix(BG, lit, arrow), cx.clip);
        }
        return;
    }
    let comet = if lp.ending { fade } else { ((grow - 0.6) / 0.4).clamp(0.0, 1.0) };
    if comet <= 0.0 {
        return;
    }
    // A soft ring at the arrowhead as the dot comes home.
    let pulse = loops::lap_pulse(t) * comet;
    if pulse > 0.01 {
        let rad = (3.0 + 7.0 * (1.0 - pulse)) * d;
        p.under(layer, Quad::rounded(end.0 - rad, end.1 - rad, 2.0 * rad, 2.0 * rad, rad, theme::rgba(lit, 0.12 * pulse), theme::rgba(lit, 0.75 * pulse), 1.2 * d, cx.clip));
    }
    // The tail: samples back in time (so it stretches where the dot is fast),
    // broken where it wraps into the previous lap (the dot went through the box).
    let mut previous = (t, path.at(loops::along(t)));
    for i in 1..=TAIL {
        let k = i as f32 / TAIL as f32;
        let when = t - TAIL_LAP * k;
        let at = path.at(loops::along(when));
        if when.floor() == previous.0.floor() {
            let alpha = 0.85 * (1.0 - k).powf(1.3) * comet;
            let w = (2.4 * (1.0 - k) + 0.9) * d;
            p.under(layer, Quad::line(previous.1, at, w, theme::rgba(theme::mix(color, lit, (1.0 - k).powf(0.7)), alpha), 0.0, cx.clip));
        }
        previous = (when, at);
    }
    // The head fades in as it leaves the box and out as it enters it.
    let s = loops::along(t);
    let edge = (s / 0.04).min((1.0 - s) / 0.04).clamp(0.0, 1.0);
    glow(p, layer, path.at(s), 2.6 * d, lit, comet * (0.25 + 0.75 * edge), cx.clip);
}

/// Each box's drawing level (`0..BOX_LEVELS`): one above the highest earlier
/// box it overlaps, so a later box covers the labels of the boxes under it.
/// Selected boxes are drawn above all levels and count as level 0 here.
pub(crate) fn box_levels(boxes: &[canvas_core::view::BoxModel]) -> Vec<usize> {
    use std::collections::HashMap;
    const BUCKET: f32 = 16.0;
    let mut buckets: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
    let mut levels = vec![0; boxes.len()];
    let span = |r: WRect| ((r.x / BUCKET).floor() as i32, (r.y / BUCKET).floor() as i32, (r.right() / BUCKET).floor() as i32, (r.bottom() / BUCKET).floor() as i32);
    let overlap = |a: WRect, b: WRect| a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom();
    for (i, b) in boxes.iter().enumerate() {
        if b.selected {
            continue;
        }
        let (x0, y0, x1, y1) = span(b.rect);
        let mut level = 0;
        for bx in x0..=x1 {
            for by in y0..=y1 {
                for &j in buckets.get(&(bx, by)).into_iter().flatten() {
                    if overlap(boxes[j].rect, b.rect) {
                        level = level.max(levels[j] + 1);
                    }
                }
            }
        }
        levels[i] = level.min(scene::BOX_LEVELS - 1);
        for bx in x0..=x1 {
            for by in y0..=y1 {
                buckets.entry((bx, by)).or_default().push(i);
            }
        }
    }
    levels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_boxes_go_up_a_level() {
        let model = |id: &str, x: f32, y: f32| canvas_core::view::BoxModel {
            id: id.into(),
            kind: BoxKind::Session,
            rect: WRect::new(x, y, 30.0, 4.0),
            prefix: "",
            name: id.into(),
            status: canvas_core::status::Status::Idle,
            icon: "",
            label: "",
            line: String::new(),
            line_style: Style::default(),
            name_style: Style::default(),
            elsewhere: false,
            subtitle: String::new(),
            border: Border::Round,
            border_style: Style::default(),
            fill: Color::Reset,
            attention: None,
            selected: false,
            focused_term: false,
            active: false,
            every: None,
            mailbox: 0,
            dust: 0,
            alpha: 1.0,
            deck: None,
            hidden: None,
        };
        let boxes = [model("a", 0.0, 0.0), model("b", 10.0, 2.0), model("c", 100.0, 0.0), model("d", 20.0, 3.0), model("e", 25.0, 3.5)];
        assert_eq!(box_levels(&boxes), [0, 1, 0, 2, 2], "capped at the top level");
    }

    #[test]
    fn points_along_a_polyline() {
        let path = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)];
        assert_eq!(along(&path, 0.0), Some((0.0, 0.0)));
        assert_eq!(along(&path, 0.5), Some((10.0, 0.0)));
        assert_eq!(along(&path, 0.75), Some((10.0, 5.0)));
        assert_eq!(along(&path, 1.0), Some((10.0, 10.0)));
        assert_eq!(style_color(Style::default().fg(Color::Rgb(200, 0, 0)), Color::Reset), [200, 0, 0]);
    }

    fn loop_model(motion: Motion, scale: f32) -> CanvasModel {
        CanvasModel {
            area: ratatui::layout::Rect::new(0, 0, 120, 40),
            origin: (0.0, 0.0),
            scale,
            grid: true,
            groups: Vec::new(),
            edges: Vec::new(),
            boxes: Vec::new(),
            loops: vec![LoopModel { id: "s".into(), rect: WRect::new(2.0, 2.0, 30.0, 4.0), halo: false, changed_ms: None, ending: false, seed: loops::seed("s") }],
            pulses: Vec::new(),
            halo: None,
            rubber: None,
            empty: false,
            tick: 0,
            effects: Vec::new(),
            motion,
            clock_ms: 0,
            frozen: false,
        }
    }

    #[test]
    fn the_loop_dot_glides_along_the_curve_at_any_frame_rate() {
        let grid = Grid::new(0.6, 1.25);
        let model = loop_model(Motion::On, 1.0);
        let cx = Ctx { model: &model, grid: &grid, clip: [0.0, 0.0, 2000.0, 1000.0], dpi: 1.25 };
        let lp = &model.loops[0];
        let path = loop_path(&cx, cx.rect(lp.rect), false).expect("an arc");
        let r = cx.rect(lp.rect);
        assert!(path.points.iter().all(|p| p.0 >= r[0] + r[2] - 0.01), "beside the box, on its right");
        // At 60 frames/s the dot moves under 2 px a frame (sub-pixel positions, no jumps),
        // except where a lap ends and the next one starts (it enters the box at the
        // bottom and leaves it at the top).
        let max = path.total() * 1.36 * 16.0 / loops::LAP_MS as f32 + 0.01;
        let mut last = loop_dot(&cx, lp, 50_000).unwrap();
        let mut fractional = false;
        for ms in (50_016..50_000 + 2 * loops::LAP_MS).step_by(16) {
            let dot = loop_dot(&cx, lp, ms).unwrap();
            let step = (dot.0 - last.0).hypot(dot.1 - last.1);
            let wrapped = lp.lap_at(ms) < lp.lap_at(ms - 16);
            assert!(wrapped || step <= max, "{step:.2} px at {ms} ms (max {max:.2})");
            assert!(wrapped || step > 0.0, "it never stops");
            fractional |= dot.0.fract() != 0.0 || dot.1.fract() != 0.0;
            last = dot;
        }
        assert!(fractional && max < 2.5, "sub-pixel steps of at most {max:.2} px");
        // The same instant gives the same place, whatever the frame rate before it.
        assert_eq!(loop_dot(&cx, lp, 61_234), loop_dot(&cx, lp, 61_234));
        // About 2 px a frame: some 30 frames/s for this arc, fewer when zoomed out.
        let near = loop_interval(&grid, &model);
        let far = loop_interval(&grid, &loop_model(Motion::On, 0.4));
        assert!((33..=45).contains(&near.as_millis()) && far > near, "{near:?} {far:?}");
        // Reduced motion: no moving dot.
        let still = loop_model(Motion::Reduced, 1.0);
        let cx = Ctx { model: &still, grid: &grid, clip: [0.0, 0.0, 2000.0, 1000.0], dpi: 1.25 };
        assert_eq!(loop_dot(&cx, &still.loops[0], 50_000), None);
        assert!(!still.loops_busy(50_000));
    }
}
