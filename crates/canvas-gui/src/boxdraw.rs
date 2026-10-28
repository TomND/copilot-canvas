//! Box-drawing and block characters drawn as pixel-aligned rectangles and
//! arcs, so borders join seamlessly whatever the font and cell height.

use crate::quads::{Quad, Rgba};

/// Weight of one arm of a box-drawing character.
const NONE: u8 = 0;
const LIGHT: u8 = 1;
const HEAVY: u8 = 2;

/// Arms (left, right, up, down) of the light and heavy lines, corners, tees and crosses.
fn arms(c: char) -> Option<[u8; 4]> {
    Some(match c {
        '─' => [1, 1, 0, 0],
        '━' => [2, 2, 0, 0],
        '│' => [0, 0, 1, 1],
        '┃' => [0, 0, 2, 2],
        '┌' => [0, 1, 0, 1],
        '┍' => [0, 2, 0, 1],
        '┎' => [0, 1, 0, 2],
        '┏' => [0, 2, 0, 2],
        '┐' => [1, 0, 0, 1],
        '┑' => [2, 0, 0, 1],
        '┒' => [1, 0, 0, 2],
        '┓' => [2, 0, 0, 2],
        '└' => [0, 1, 1, 0],
        '┕' => [0, 2, 1, 0],
        '┖' => [0, 1, 2, 0],
        '┗' => [0, 2, 2, 0],
        '┘' => [1, 0, 1, 0],
        '┙' => [2, 0, 1, 0],
        '┚' => [1, 0, 2, 0],
        '┛' => [2, 0, 2, 0],
        '├' => [0, 1, 1, 1],
        '┣' => [0, 2, 2, 2],
        '┤' => [1, 0, 1, 1],
        '┫' => [2, 0, 2, 2],
        '┬' => [1, 1, 0, 1],
        '┳' => [2, 2, 0, 2],
        '┴' => [1, 1, 1, 0],
        '┻' => [2, 2, 2, 0],
        '┼' => [1, 1, 1, 1],
        '╋' => [2, 2, 2, 2],
        '╴' => [1, 0, 0, 0],
        '╵' => [0, 0, 1, 0],
        '╶' => [0, 1, 0, 0],
        '╷' => [0, 0, 0, 1],
        '╸' => [2, 0, 0, 0],
        '╹' => [0, 0, 2, 0],
        '╺' => [0, 2, 0, 0],
        '╻' => [0, 0, 0, 2],
        '╼' => [1, 2, 0, 0],
        '╾' => [2, 1, 0, 0],
        _ => return None,
    })
}

/// Dashed lines: (vertical, dashes per cell, weight).
fn dashes(c: char) -> Option<(bool, u8, u8)> {
    Some(match c {
        '╌' => (false, 2, LIGHT),
        '╍' => (false, 2, HEAVY),
        '┄' => (false, 3, LIGHT),
        '┅' => (false, 3, HEAVY),
        '┈' => (false, 4, LIGHT),
        '┉' => (false, 4, HEAVY),
        '╎' => (true, 2, LIGHT),
        '╏' => (true, 2, HEAVY),
        '┆' => (true, 3, LIGHT),
        '┇' => (true, 3, HEAVY),
        '┊' => (true, 4, LIGHT),
        '┋' => (true, 4, HEAVY),
        _ => return None,
    })
}

/// Characters `draw` handles.
pub fn is_drawn(c: char) -> bool {
    arms(c).is_some()
        || dashes(c).is_some()
        || matches!(c, '═' | '║' | '╔' | '╗' | '╚' | '╝' | '╭' | '╮' | '╯' | '╰' | '█' | '▀' | '▔' | '▐' | '▕' | '░' | '▒' | '▓')
        || matches!(c, '\u{2581}'..='\u{2587}' | '\u{2589}'..='\u{258f}')
}

/// Line thickness for a weight at this cell width.
fn thickness(weight: u8, w: f32) -> f32 {
    let light = (w / 8.0).round().max(1.0);
    match weight {
        HEAVY => (light * 2.0).max(2.0),
        _ => light,
    }
}

fn rect(x0: f32, y0: f32, x1: f32, y1: f32, color: Rgba, clip: [f32; 4], out: &mut Vec<Quad>) {
    if x1 > x0 && y1 > y0 {
        out.push(Quad::rect(x0, y0, x1 - x0, y1 - y0, color, clip));
    }
}

/// Draws `c` into the cell at `(x, y)` of size `w` × `h` (whole pixels) when
/// it is a box-drawing or block character; false for anything else.
#[allow(clippy::too_many_arguments)]
pub fn draw(c: char, x: f32, y: f32, w: f32, h: f32, color: Rgba, clip: [f32; 4], out: &mut Vec<Quad>) -> bool {
    let (x, y) = (x.round(), y.round());
    let (x1, y1) = ((x + w).round(), (y + h).round());
    let (w, h) = (x1 - x, y1 - y);
    // The start of a line of thickness `t` centred in the cell.
    let mid_x = |t: f32| (x + (w - t) / 2.0).floor();
    let mid_y = |t: f32| (y + (h - t) / 2.0).floor();
    if let Some([left, right, up, down]) = arms(c) {
        let tv = thickness(up.max(down), w);
        let th = thickness(left.max(right), w);
        let has_v = up.max(down) > NONE;
        let has_h = left.max(right) > NONE;
        for (weight, horizontal, positive) in [(left, true, false), (right, true, true), (up, false, false), (down, false, true)] {
            if weight == NONE {
                continue;
            }
            let t = thickness(weight, w);
            if horizontal {
                // Horizontal arms reach across the vertical line's width.
                let (cx0, cx1) = if has_v { (mid_x(tv), mid_x(tv) + tv) } else { (mid_x(t), mid_x(t) + t) };
                let y0 = mid_y(t);
                if positive { rect(cx0, y0, x1, y0 + t, color, clip, out) } else { rect(x, y0, cx1, y0 + t, color, clip, out) }
            } else {
                let (cy0, cy1) = if has_h { (mid_y(th), mid_y(th) + th) } else { (mid_y(t), mid_y(t) + t) };
                let x0 = mid_x(t);
                if positive { rect(x0, cy0, x0 + t, y1, color, clip, out) } else { rect(x0, y, x0 + t, cy1, color, clip, out) }
            }
        }
        return true;
    }
    if let Some((vertical, count, weight)) = dashes(c) {
        let t = thickness(weight, w);
        let n = count as f32;
        let (length, start) = if vertical { (h, y) } else { (w, x) };
        let step = length / n;
        for i in 0..count {
            let a = (start + i as f32 * step + step * 0.2).round();
            let b = (start + i as f32 * step + step * 0.8).round();
            if vertical {
                rect(mid_x(t), a, mid_x(t) + t, b, color, clip, out);
            } else {
                rect(a, mid_y(t), b, mid_y(t) + t, color, clip, out);
            }
        }
        return true;
    }
    let t = thickness(LIGHT, w);
    // Double lines: two light lines `d` from the centre.
    let d = (w / 5.0).round().max(t + 1.0);
    let (cx, cy) = (mid_x(t), mid_y(t));
    match c {
        '═' => {
            rect(x, cy - d, x1, cy - d + t, color, clip, out);
            rect(x, cy + d, x1, cy + d + t, color, clip, out);
            return true;
        }
        '║' => {
            rect(cx - d, y, cx - d + t, y1, color, clip, out);
            rect(cx + d, y, cx + d + t, y1, color, clip, out);
            return true;
        }
        '╔' | '╗' | '╚' | '╝' => {
            // Outer and inner line of each arm; `sx`/`sy`: towards the arms.
            let (sx, sy) = match c {
                '╔' => (1.0, 1.0),
                '╗' => (-1.0, 1.0),
                '╚' => (1.0, -1.0),
                _ => (-1.0, -1.0),
            };
            for offset in [-d, d] {
                // The horizontal line at `cy + offset * sy`, the vertical at `cx + offset * sx`.
                let hy = cy + offset * sy;
                let vx = cx + offset * sx;
                let (hx0, hx1) = if sx > 0.0 { (vx, x1) } else { (x, vx + t) };
                let (vy0, vy1) = if sy > 0.0 { (hy, y1) } else { (y, hy + t) };
                rect(hx0, hy, hx1, hy + t, color, clip, out);
                rect(vx, vy0, vx + t, vy1, color, clip, out);
            }
            return true;
        }
        '╭' | '╮' | '╯' | '╰' => {
            let r = (w.min(h) / 2.0).floor().max(1.0);
            let big = 2.0 * (r + t) + w.max(h) * 2.0;
            let (rx, ry) = match c {
                '╭' => (cx, cy),
                '╮' => (cx + t - big, cy),
                '╯' => (cx + t - big, cy + t - big),
                _ => (cx, cy + t - big),
            };
            let cell = [x.max(clip[0]), y.max(clip[1]), x1.min(clip[2]), y1.min(clip[3])];
            out.push(Quad::rounded(rx, ry, big, big, r + t / 2.0, [0.0; 4], color, t, cell));
            return true;
        }
        _ => {}
    }
    // Block elements.
    let eighth_h = |n: f32| (h * n / 8.0).round();
    let eighth_w = |n: f32| (w * n / 8.0).round();
    match c {
        '█' => rect(x, y, x1, y1, color, clip, out),
        '▀' => rect(x, y, x1, y + eighth_h(4.0), color, clip, out),
        '▔' => rect(x, y, x1, y + eighth_h(1.0), color, clip, out),
        '▐' => rect(x1 - eighth_w(4.0), y, x1, y1, color, clip, out),
        '▕' => rect(x1 - eighth_w(1.0), y, x1, y1, color, clip, out),
        '\u{2581}'..='\u{2587}' => {
            let n = (c as u32 - 0x2580) as f32;
            rect(x, y1 - eighth_h(n), x1, y1, color, clip, out);
        }
        '\u{2589}'..='\u{258f}' => {
            let n = (0x2590 - c as u32) as f32;
            rect(x, y, x + eighth_w(n).max(1.0), y1, color, clip, out);
        }
        '░' | '▒' | '▓' => {
            let alpha = match c {
                '░' => 0.25,
                '▒' => 0.5,
                _ => 0.75,
            };
            rect(x, y, x1, y1, [color[0], color[1], color[2], color[3] * alpha], clip, out);
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quads::NO_CLIP;

    fn quads(c: char) -> Vec<Quad> {
        let mut out = Vec::new();
        assert!(draw(c, 10.0, 20.0, 8.0, 18.0, [1.0; 4], NO_CLIP, &mut out), "{c}");
        out
    }

    fn covers(q: &[Quad], px: f32, py: f32) -> bool {
        q.iter().any(|q| px >= q.rect[0] && px < q.rect[0] + q.rect[2] && py >= q.rect[1] && py < q.rect[1] + q.rect[3])
    }

    #[test]
    fn lines_reach_the_cell_edges_and_join_at_the_centre() {
        let h = quads('─');
        assert!(covers(&h, 10.0, 28.5) && covers(&h, 17.9, 28.5) && !covers(&h, 13.0, 20.5));
        let corner = quads('┌');
        assert!(covers(&corner, 17.9, 28.5) && covers(&corner, 13.5, 37.9), "right and down arms reach the edges");
        assert!(!covers(&corner, 10.5, 28.5) && !covers(&corner, 13.5, 20.5), "no left or up arm");
        let heavy = quads('━');
        assert_eq!(heavy[0].rect[3], 2.0);
        assert_eq!(quads('┄').len(), 3);
        assert_eq!(quads('║').len(), 2);
        assert_eq!(quads('╔').len(), 4);
        assert_eq!(quads('╭').len(), 1);
        assert!(covers(&quads('▌'), 10.5, 21.0) && !covers(&quads('▌'), 15.5, 21.0));
        let mut out = Vec::new();
        assert!(!draw('a', 0.0, 0.0, 8.0, 18.0, [1.0; 4], NO_CLIP, &mut out));
    }
}
