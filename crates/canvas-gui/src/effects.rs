//! Effects from the effects collection, drawn with the quad pipeline:
//! fireworks bursts, attention rings, sparkles and alert flashes (animated
//! on the spinners' 80 ms clock), their static forms for reduced motion, and the
//! badge every mode shows (so an effect is never lost, even when they are
//! off). Each effect is clipped to a region around its anchor.

use canvas_core::paint;
use canvas_core::view::{EffectModel, Motion};

use crate::canvas::{Ctx, glow_dot, noise, pill, rgb, seed};
use crate::quads::Quad;
use crate::scene::{self, Painter};
use crate::theme;

/// Okabe-Ito colours for the fireworks.
const SPARKS: [[u8; 3]; 6] = [[86, 180, 233], [230, 159, 0], [32, 190, 150], [204, 121, 167], [240, 228, 66], [230, 110, 40]];
const TAU: f32 = std::f32::consts::TAU;

/// The colour of an effect kind (its badge and rings).
pub fn kind_color(kind: &str) -> [u8; 3] {
    match kind {
        "fireworks" => rgb(paint::POSITIVE),
        "attention" => rgb(paint::ATTENTION),
        "alert" => rgb(paint::NEGATIVE),
        _ => rgb(paint::LOOP_LIT),
    }
}

/// Draws one effect on its anchor: animated (`On`), still (`Reduced`) or as
/// its badge alone (`Off`).
pub fn paint(p: &mut Painter, cx: &Ctx, effect: &EffectModel, motion: Motion) {
    let r = cx.rect(effect.rect);
    let (ux, uy) = cx.unit();
    if !cx.visible(r, 8.0 * ux.max(uy)) {
        return;
    }
    // Readable at any zoom: distances never fall below a few pixels.
    let u = ux.max(5.0 * cx.dpi);
    let v = uy.max(9.0 * cx.dpi);
    // A group's effect plays over its title row.
    let focus = if effect.group { [r[0], r[1], r[2].min(30.0 * u), 2.0 * v] } else { r };
    let bounds = [
        (focus[0] - 6.0 * u).max(cx.clip[0]),
        (focus[1] - 5.0 * v).max(cx.clip[1]),
        (focus[0] + focus[2] + 6.0 * u).min(cx.clip[2]),
        (focus[1] + focus[3] + 2.0 * v).min(cx.clip[3]),
    ];
    let color = kind_color(effect.kind);
    let t = effect.age_ms as f32;
    // Seeded by what it shows (stable across runs and windows).
    let s = seed(&format!("{}|{}", effect.kind, effect.badge));
    let layer = scene::CANVAS_TOP;
    match (motion, effect.kind) {
        (Motion::Off, _) => {}
        (Motion::On, "fireworks") => fireworks(p, cx, layer, focus, bounds, t, s, (u, v)),
        (Motion::On, "attention") => attention(p, cx, layer, focus, bounds, t, color, u),
        (Motion::On, "alert") => {
            let a = 0.35 + 0.35 * (TAU * t / 900.0).sin();
            ring(p, layer, focus, 2.0 * cx.dpi, theme::rgba(color, a), 3.0 * cx.dpi, cx, bounds);
        }
        (Motion::On, _) => sparkle(p, cx, layer, focus, bounds, t, s, color),
        (Motion::Reduced, "fireworks") => starburst(p, cx, layer, (focus[0] + focus[2], focus[1]), bounds),
        (Motion::Reduced, "attention") => {
            ring(p, layer, focus, 3.0 * cx.dpi, theme::rgba(color, 0.85), 2.0 * cx.dpi, cx, bounds);
            ring(p, layer, focus, 7.0 * cx.dpi, theme::rgba(color, 0.4), 1.5 * cx.dpi, cx, bounds);
        }
        (Motion::Reduced, "alert") => ring(p, layer, focus, 3.0 * cx.dpi, theme::rgba(color, 0.8), 2.5 * cx.dpi, cx, bounds),
        (Motion::Reduced, _) => {
            for i in 0..3 {
                let (x, y) = edge_point(focus, noise(s, i), cx.dpi * 3.0);
                star(p, layer, (x, y), 5.0 * cx.dpi, theme::rgba(color, 0.9), cx.dpi, bounds);
            }
        }
    }
    if cx.model.scale >= 0.15 {
        pill(p, cx, (r[0] + 1.5 * ux.max(3.0 * cx.dpi), r[1] + r[3]), false, &effect.badge, color);
    }
}

/// A rounded ring `grow` px outside a rect.
#[allow(clippy::too_many_arguments)]
fn ring(p: &mut Painter, layer: usize, r: [f32; 4], grow: f32, color: [f32; 4], width: f32, cx: &Ctx, clip: [f32; 4]) {
    let radius = (0.45 * cx.unit().1).clamp(2.0, 10.0 * cx.dpi) + grow;
    p.under(layer, Quad::rounded(r[0] - grow, r[1] - grow, r[2] + 2.0 * grow, r[3] + 2.0 * grow, radius, [0.0; 4], color, width, clip));
}

/// A four-pointed star.
fn star(p: &mut Painter, layer: usize, (x, y): (f32, f32), size: f32, color: [f32; 4], dpi: f32, clip: [f32; 4]) {
    if size < 0.5 {
        return;
    }
    let w = (0.9 * dpi).max(1.0);
    p.under(layer, Quad::line((x - size, y), (x + size, y), w, color, 0.0, clip));
    p.under(layer, Quad::line((x, y - size), (x, y + size), w, color, 0.0, clip));
    let d = size * 0.45;
    p.under(layer, Quad::line((x - d, y - d), (x + d, y + d), w * 0.8, color, 0.0, clip));
    p.under(layer, Quad::line((x - d, y + d), (x + d, y - d), w * 0.8, color, 0.0, clip));
}

/// A point on a rect's outline (`t`: 0..1 around it), pushed out by `out` px.
fn edge_point(r: [f32; 4], t: f32, out: f32) -> (f32, f32) {
    let perimeter = 2.0 * (r[2] + r[3]);
    let mut d = t.rem_euclid(1.0) * perimeter;
    if d < r[2] {
        return (r[0] + d, r[1] - out);
    }
    d -= r[2];
    if d < r[3] {
        return (r[0] + r[2] + out, r[1] + d);
    }
    d -= r[3];
    if d < r[2] {
        return (r[0] + r[2] - d, r[1] + r[3] + out);
    }
    (r[0] - out, r[1] + r[3] - (d - r[2]))
}

/// Three rockets in turn rise from the box and burst into coloured sparks
/// that fall and fade.
#[allow(clippy::too_many_arguments)]
fn fireworks(p: &mut Painter, cx: &Ctx, layer: usize, r: [f32; 4], clip: [f32; 4], t: f32, s: u32, (u, v): (f32, f32)) {
    const PERIOD: f32 = 1_800.0;
    const RISE: f32 = 0.22;
    let dpi = cx.dpi;
    for k in 0..3u32 {
        let local = t - k as f32 * 600.0;
        if local < 0.0 {
            continue;
        }
        let cycle = (local / PERIOD).floor() as u32;
        let phase = (local % PERIOD) / PERIOD;
        let rs = s ^ cycle.wrapping_mul(7919).wrapping_add(k * 104_729);
        let x = r[0] + r[2] * (0.15 + 0.7 * noise(rs, 1));
        let apex = r[1] - v * (0.8 + 2.4 * noise(rs, 2));
        let base = r[1] + r[3] * 0.4;
        let color = SPARKS[(noise(rs, 3) * 6.0) as usize % 6];
        if phase < RISE {
            let q = phase / RISE;
            let y = base + (apex - base) * (1.0 - (1.0 - q) * (1.0 - q));
            p.under(layer, Quad::line((x, y + 0.7 * v), (x, y), 1.4 * dpi, theme::rgba(color, 0.45), 0.0, clip));
            glow_dot(p, layer, (x, y), 1.8 * dpi, [255, 244, 214], clip);
            continue;
        }
        let q = (phase - RISE) / (1.0 - RISE);
        let fade = (1.0 - q).powf(1.4);
        if q < 0.14 {
            let flash = 1.0 - q / 0.14;
            p.under(layer, Quad::rounded(x - 6.0 * dpi, apex - 6.0 * dpi, 12.0 * dpi, 12.0 * dpi, 6.0 * dpi, theme::rgba([255, 250, 230], 0.45 * flash), [0.0; 4], 0.0, clip));
        }
        let spread = 1.0 - (1.0 - q) * (1.0 - q);
        let fall = q * q * 1.3 * v;
        for i in 0..20u32 {
            let a = TAU * (i as f32 + noise(rs, 10 + i) * 0.5) / 20.0;
            let reach = (3.2 + 2.2 * noise(rs, 40 + i)) * u;
            let at = |f: f32| (x + a.cos() * reach * spread * f, apex + a.sin() * reach * spread * f * 0.75 + fall * f);
            let spark = if i % 3 == 0 { [255, 244, 214] } else { color };
            p.under(layer, Quad::line(at(0.72), at(1.0), 1.5 * dpi, theme::rgba(spark, 0.55 * fade), 0.0, clip));
            let d = (2.1 * dpi * (0.4 + 0.6 * fade)).max(0.8);
            let (px, py) = at(1.0);
            p.under(layer, Quad::rounded(px - d, py - d, 2.0 * d, 2.0 * d, d, theme::rgba(spark, fade), [0.0; 4], 0.0, clip));
        }
    }
}

/// Rings pulse out of the box and a beacon blinks on its corner.
#[allow(clippy::too_many_arguments)]
fn attention(p: &mut Painter, cx: &Ctx, layer: usize, r: [f32; 4], clip: [f32; 4], t: f32, color: [u8; 3], u: f32) {
    const PERIOD: f32 = 1_500.0;
    for k in 0..2 {
        let phase = ((t + k as f32 * PERIOD / 2.0) % PERIOD) / PERIOD;
        let grow = 2.0 * cx.dpi + phase * 1.6 * u;
        let alpha = (1.0 - phase).powf(1.5) * 0.9;
        ring(p, layer, r, grow, theme::rgba(color, alpha), 2.0 * cx.dpi, cx, clip);
    }
    let pulse = 0.5 + 0.5 * (TAU * t / PERIOD).sin();
    glow_dot(p, layer, (r[0] + r[2], r[1]), (2.4 + 1.4 * pulse) * cx.dpi, color, clip);
}

/// Stars twinkle around the box (and fade out at the end of a sparkle's life).
#[allow(clippy::too_many_arguments)]
fn sparkle(p: &mut Painter, cx: &Ctx, layer: usize, r: [f32; 4], clip: [f32; 4], t: f32, s: u32, color: [u8; 3]) {
    let end = (1.0 - ((t - 5_000.0) / 1_000.0).clamp(0.0, 1.0)).max(0.0);
    for i in 0..7u32 {
        let (x, y) = edge_point(r, noise(s, i), 2.0 * cx.dpi + noise(s, i + 50) * 6.0 * cx.dpi);
        let twinkle = (TAU * (t / 700.0 + noise(s, i + 90))).sin().max(0.0);
        let tint = if i % 2 == 0 { [255, 250, 230] } else { color };
        star(p, layer, (x, y), 6.0 * cx.dpi * twinkle, theme::rgba(tint, end), cx.dpi, clip);
    }
}

/// The still form of fireworks: a starburst on the box's corner.
fn starburst(p: &mut Painter, cx: &Ctx, layer: usize, (x, y): (f32, f32), clip: [f32; 4]) {
    for i in 0..10 {
        let a = TAU * i as f32 / 10.0;
        let len = if i % 2 == 0 { 11.0 } else { 7.0 } * cx.dpi;
        let color = SPARKS[i % SPARKS.len()];
        p.under(layer, Quad::line((x + a.cos() * 3.0 * cx.dpi, y + a.sin() * 3.0 * cx.dpi), (x + a.cos() * len, y + a.sin() * len), 1.6 * cx.dpi, theme::rgba(color, 0.95), 0.0, clip));
    }
    glow_dot(p, layer, (x, y), 2.2 * cx.dpi, [255, 244, 214], clip);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_points_walk_around_the_rect() {
        let r = [0.0, 0.0, 10.0, 5.0];
        assert_eq!(edge_point(r, 0.0, 0.0), (0.0, 0.0));
        assert_eq!(edge_point(r, 0.5, 1.0), (10.0, 6.0), "half way: the bottom right corner, pushed out");
        assert_eq!(edge_point(r, 0.25, 0.0), (7.5, 0.0));
        let (x, y) = edge_point(r, 0.99, 0.0);
        assert!(x == 0.0 && y > 0.0 && y < 1.0);
        assert_eq!(kind_color("attention"), rgb(paint::ATTENTION));
    }
}
