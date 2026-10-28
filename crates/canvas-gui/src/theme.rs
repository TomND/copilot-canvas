//! Colours: the canvas palette (from canvas-core's `paint`), terminal colours
//! and conversions to the GPU's straight-alpha floats.

use ratatui::style::Color;

use crate::quads::Rgba;

/// Window and canvas background.
pub const BG: [u8; 3] = [13, 17, 23];
/// Default text colour (`paint::TEXT`).
pub const FG: [u8; 3] = [201, 209, 217];

/// The 16 ANSI colours (Windows Terminal "Campbell").
const ANSI: [u32; 16] = [
    0x0c0c0c, 0xc50f1f, 0x13a10e, 0xc19c00, 0x0037da, 0x881798, 0x3a96dd, 0xcccccc, 0x767676, 0xe74856, 0x16c60c, 0xf9f1a5, 0x3b78ff, 0xb4009e,
    0x61d6d6, 0xf2f2f2,
];

fn split(c: u32) -> [u8; 3] {
    [(c >> 16) as u8, (c >> 8) as u8, c as u8]
}

/// Colour `index` of the 256-colour palette.
pub fn indexed(index: u8) -> [u8; 3] {
    match index {
        0..=15 => split(ANSI[index as usize]),
        16..=231 => {
            let i = index - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            [level(i / 36), level((i / 6) % 6), level(i % 6)]
        }
        _ => {
            let v = 8 + (index - 232) * 10;
            [v, v, v]
        }
    }
}

/// A ratatui colour as RGB; `Reset` is `default`.
pub fn rgb(color: Color, default: [u8; 3]) -> [u8; 3] {
    match color {
        Color::Reset => default,
        Color::Rgb(r, g, b) => [r, g, b],
        Color::Indexed(i) => indexed(i),
        Color::Black => indexed(0),
        Color::Red => indexed(1),
        Color::Green => indexed(2),
        Color::Yellow => indexed(3),
        Color::Blue => indexed(4),
        Color::Magenta => indexed(5),
        Color::Cyan => indexed(6),
        Color::Gray => indexed(7),
        Color::DarkGray => indexed(8),
        Color::LightRed => indexed(9),
        Color::LightGreen => indexed(10),
        Color::LightYellow => indexed(11),
        Color::LightBlue => indexed(12),
        Color::LightMagenta => indexed(13),
        Color::LightCyan => indexed(14),
        Color::White => indexed(15),
    }
}

/// A protocol colour (0 default, `COLOR_RGB` bit, else palette index + 1).
pub fn protocol(value: u32, default: [u8; 3]) -> [u8; 3] {
    use canvas_common::protocol::{COLOR_DEFAULT, COLOR_RGB};
    if value == COLOR_DEFAULT {
        default
    } else if value & COLOR_RGB != 0 {
        split(value & 0xff_ffff)
    } else {
        indexed(((value - 1) & 0xff) as u8)
    }
}

/// `a` blended towards `b` by `t` (0: a, 1: b).
pub fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    let ch = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    [ch(a[0], b[0]), ch(a[1], b[1]), ch(a[2], b[2])]
}

pub fn rgba(c: [u8; 3], alpha: f32) -> Rgba {
    [c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0, alpha]
}

/// A ratatui colour as straight-alpha floats.
pub fn color(color: Color, alpha: f32) -> Rgba {
    rgba(rgb(color, FG), alpha)
}

pub fn text_color(c: [u8; 3]) -> glyphon::Color {
    glyphon::Color::rgb(c[0], c[1], c[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_and_protocol_colours_resolve() {
        use canvas_common::protocol::COLOR_RGB;
        assert_eq!(protocol(0, [1, 2, 3]), [1, 2, 3]);
        assert_eq!(protocol(COLOR_RGB | 0xabcdef, BG), [0xab, 0xcd, 0xef]);
        assert_eq!(protocol(2, BG), split(ANSI[1]), "index 1 is sent as 2");
        assert_eq!(indexed(16), [0, 0, 0]);
        assert_eq!(indexed(231), [255, 255, 255]);
        assert_eq!(indexed(255), [238, 238, 238]);
        assert_eq!(rgb(Color::Rgb(1, 2, 3), BG), [1, 2, 3]);
        assert_eq!(rgb(Color::Reset, BG), BG);
        assert_eq!(mix([0, 0, 0], [200, 100, 50], 0.5), [100, 50, 25]);
    }
}
