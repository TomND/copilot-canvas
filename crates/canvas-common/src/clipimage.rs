//! Clipboard images between machines: a device-independent bitmap (`CF_DIB`
//! / `CF_DIBV5`, what screenshots put on the clipboard) to PNG on the side
//! that copies, and PNG back to a 32-bit `CF_DIB` on the side that pastes.

use std::io::Cursor;

use anyhow::{Context, Result, bail, ensure};

/// Largest image sent to a remote service's clipboard (PNG bytes).
pub const MAX_PNG: usize = 20 << 20;
/// Largest bitmap accepted on either side (pixels; 128 MiB as 32-bit pixels).
const MAX_PIXELS: u64 = 32 << 20;

const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// Channel value of `pixel` under `mask`, scaled to 8 bits.
fn channel(pixel: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let value = (pixel & mask) >> mask.trailing_zeros();
    let max = mask >> mask.trailing_zeros();
    ((value as u64 * 255 + max as u64 / 2) / max as u64) as u8
}

/// A packed DIB (`BITMAPINFOHEADER` or a later header, then pixels) as PNG.
/// 24- and 32-bit bitmaps, uncompressed or with bit fields.
pub fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>> {
    let header = u32_at(dib, 0).context("empty bitmap")? as usize;
    ensure!(header >= 40 && dib.len() > header, "not a device-independent bitmap");
    let width = u32_at(dib, 4).unwrap_or(0) as i32;
    let height = u32_at(dib, 8).unwrap_or(0) as i32;
    let bits = u16_at(dib, 14).unwrap_or(0);
    let compression = u32_at(dib, 16).unwrap_or(0);
    let colors_used = u32_at(dib, 32).unwrap_or(0) as usize;
    ensure!(width > 0 && height != 0, "the bitmap has no pixels");
    let (w, h) = (width as usize, height.unsigned_abs() as usize);
    ensure!((w as u64) * (h as u64) <= MAX_PIXELS, "the bitmap is too large");
    let mut offset = header;
    let masks = match (compression, bits) {
        (BI_RGB, 32) => [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0xff00_0000],
        (BI_RGB, 24) => [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0],
        (BI_BITFIELDS, 32) => {
            // The masks follow a plain info header; later headers hold them at the same offsets.
            let alpha = if header >= 56 { u32_at(dib, 52).unwrap_or(0) } else { 0 };
            let masks = [u32_at(dib, 40).unwrap_or(0), u32_at(dib, 44).unwrap_or(0), u32_at(dib, 48).unwrap_or(0), alpha];
            if header == 40 {
                offset += 12;
            }
            masks
        }
        _ => bail!("unsupported bitmap format ({bits} bits per pixel, compression {compression})"),
    };
    // Bitmaps of 24 or 32 bits normally have no palette, but may list one.
    offset += colors_used * 4;
    let bytes_per_pixel = bits as usize / 8;
    let stride = (w * bytes_per_pixel).div_ceil(4) * 4;
    ensure!(dib.len() >= offset + stride * h, "the bitmap is truncated");
    let pixels = &dib[offset..];
    let mut rgba = vec![0u8; w * h * 4];
    let mut any_alpha = false;
    for y in 0..h {
        // Positive heights are stored bottom-up.
        let source = if height > 0 { h - 1 - y } else { y };
        let row = &pixels[source * stride..source * stride + w * bytes_per_pixel];
        for x in 0..w {
            let p = &row[x * bytes_per_pixel..(x + 1) * bytes_per_pixel];
            let pixel = if bytes_per_pixel == 4 { u32::from_le_bytes([p[0], p[1], p[2], p[3]]) } else { u32::from_le_bytes([p[0], p[1], p[2], 0]) };
            let out = &mut rgba[(y * w + x) * 4..(y * w + x + 1) * 4];
            out[0] = channel(pixel, masks[0]);
            out[1] = channel(pixel, masks[1]);
            out[2] = channel(pixel, masks[2]);
            out[3] = channel(pixel, masks[3]);
            any_alpha |= out[3] != 0;
        }
    }
    // Most 32-bit clipboard bitmaps leave the alpha byte at 0: they are opaque.
    if !any_alpha {
        for pixel in rgba.chunks_mut(4) {
            pixel[3] = 255;
        }
    }
    encode_png(w as u32, h as u32, &rgba)
}

/// RGBA pixels as a PNG.
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().context("cannot encode the image")?;
    writer.write_image_data(rgba).context("cannot encode the image")?;
    writer.finish().context("cannot encode the image")?;
    Ok(out)
}

/// A decoded PNG frame: (width, height, colour type, rows as decoded).
fn decode_frame(png: &[u8]) -> Result<(u32, u32, png::ColorType, Vec<u8>)> {
    let mut decoder = png::Decoder::new(Cursor::new(png));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().context("not a PNG image")?;
    let (width, height) = (reader.info().width, reader.info().height);
    ensure!(width > 0 && height > 0 && (width as u64) * (height as u64) <= MAX_PIXELS, "the image is empty or too large");
    let mut buf = vec![0u8; reader.output_buffer_size().context("the image is too large")?];
    let frame = reader.next_frame(&mut buf).context("the PNG image is damaged")?;
    buf.truncate(frame.buffer_size());
    let channels = match frame.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Grayscale => 1,
        other => bail!("unsupported PNG colour type {other:?}"),
    };
    ensure!(buf.len() == (width * height) as usize * channels, "the PNG image is damaged");
    Ok((width, height, frame.color_type, buf))
}

/// Calls `pixel` with each (r, g, b, a) of decoded rows in order.
fn each_pixel(color: png::ColorType, data: &[u8], mut pixel: impl FnMut([u8; 4])) {
    match color {
        png::ColorType::Rgba => data.as_chunks::<4>().0.iter().for_each(|&[r, g, b, a]| pixel([r, g, b, a])),
        png::ColorType::Rgb => data.as_chunks::<3>().0.iter().for_each(|&[r, g, b]| pixel([r, g, b, 255])),
        png::ColorType::GrayscaleAlpha => data.as_chunks::<2>().0.iter().for_each(|&[g, a]| pixel([g, g, g, a])),
        _ => data.iter().for_each(|&g| pixel([g, g, g, 255])),
    }
}

/// A PNG as (width, height, RGBA pixels).
pub fn decode_png(png: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let (width, height, color, data) = decode_frame(png)?;
    if color == png::ColorType::Rgba {
        return Ok((width, height, data));
    }
    let mut rgba = Vec::with_capacity((width * height) as usize * 4);
    each_pixel(color, &data, |p| rgba.extend_from_slice(&p));
    Ok((width, height, rgba))
}

/// A PNG as a packed 32-bit top-down `CF_DIB` (`BITMAPINFOHEADER` + BGRA
/// rows), converted straight from the decoded rows into the bitmap.
pub fn png_to_dib(png: &[u8]) -> Result<Vec<u8>> {
    let (width, height, color, data) = decode_frame(png)?;
    let pixels = (width * height) as usize * 4;
    let mut dib = Vec::with_capacity(40 + pixels);
    dib.extend_from_slice(&40u32.to_le_bytes());
    dib.extend_from_slice(&(width as i32).to_le_bytes());
    // Negative height: rows top-down.
    dib.extend_from_slice(&(-(height as i32)).to_le_bytes());
    dib.extend_from_slice(&1u16.to_le_bytes());
    dib.extend_from_slice(&32u16.to_le_bytes());
    dib.extend_from_slice(&BI_RGB.to_le_bytes());
    dib.extend_from_slice(&(pixels as u32).to_le_bytes());
    dib.extend_from_slice(&[0u8; 16]);
    each_pixel(color, &data, |[r, g, b, a]| dib.extend_from_slice(&[b, g, r, a]));
    Ok(dib)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 3x2 bottom-up bitmap: red, green, blue / white, black, grey.
    fn dib(bits: u16, bitfields: bool) -> Vec<u8> {
        let rows: [[[u8; 3]; 3]; 2] = [[[255, 0, 0], [0, 255, 0], [0, 0, 255]], [[255, 255, 255], [0, 0, 0], [128, 128, 128]]];
        let mut out = Vec::new();
        out.extend_from_slice(&40u32.to_le_bytes());
        out.extend_from_slice(&3i32.to_le_bytes());
        out.extend_from_slice(&2i32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(&(if bitfields { BI_BITFIELDS } else { BI_RGB }).to_le_bytes());
        out.extend_from_slice(&[0u8; 20]);
        if bitfields {
            for mask in [0x00ff_0000u32, 0x0000_ff00, 0x0000_00ff] {
                out.extend_from_slice(&mask.to_le_bytes());
            }
        }
        for row in rows.iter().rev() {
            let start = out.len();
            for [r, g, b] in row {
                out.extend_from_slice(&[*b, *g, *r]);
                if bits == 32 {
                    out.push(0);
                }
            }
            while !(out.len() - start).is_multiple_of(4) {
                out.push(0);
            }
        }
        out
    }

    #[test]
    fn bitmaps_become_png_and_back() {
        for (bits, bitfields) in [(24, false), (32, false), (32, true)] {
            let png = dib_to_png(&dib(bits, bitfields)).unwrap();
            assert_eq!(&png[1..4], b"PNG");
            let (w, h, rgba) = decode_png(&png).unwrap();
            assert_eq!((w, h), (3, 2));
            assert_eq!(&rgba[..4], &[255, 0, 0, 255], "{bits} bits: top-left red, opaque");
            assert_eq!(&rgba[20..24], &[128, 128, 128, 255], "bottom-right grey");
            let back = png_to_dib(&png).unwrap();
            assert_eq!(u32_at(&back, 8).map(|h| h as i32), Some(-2), "top-down");
            assert_eq!(&back[40..44], &[0, 0, 255, 255], "BGRA");
            assert_eq!(back.len(), 40 + 3 * 2 * 4);
            assert_eq!(dib_to_png(&back).map(|p| decode_png(&p).unwrap().2).unwrap(), rgba, "round trip");
        }
        assert!(dib_to_png(b"junk").is_err());
        assert!(decode_png(b"\x89PNG broken").is_err());
        let mut eight = dib(24, false);
        eight[14] = 8;
        assert!(dib_to_png(&eight).is_err(), "palette bitmaps are not supported");
    }

    /// A 1x1 PNG whose header claims `width` x `height` (CRC fixed up).
    fn claimed_png(width: u32, height: u32) -> Vec<u8> {
        let mut png = encode_png(1, 1, &[1, 2, 3, 255]).unwrap();
        png[16..20].copy_from_slice(&width.to_be_bytes());
        png[20..24].copy_from_slice(&height.to_be_bytes());
        let crc = png[12..29].iter().fold(!0u32, |mut crc, &byte| {
            crc ^= byte as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
            }
            crc
        });
        png[29..33].copy_from_slice(&(!crc).to_be_bytes());
        png
    }

    #[test]
    fn oversized_images_are_refused_before_their_pixels_are_allocated() {
        assert!(decode_png(&claimed_png(1, 1)).is_ok(), "the patched header is valid");
        for (w, h) in [(8192, 8192), (100_000, 400)] {
            assert!(decode_png(&claimed_png(w, h)).unwrap_err().to_string().contains("too large"), "{w}x{h}");
            assert!(png_to_dib(&claimed_png(w, h)).is_err());
        }
        let mut huge = dib(32, false);
        huge[4..8].copy_from_slice(&8192i32.to_le_bytes());
        huge[8..12].copy_from_slice(&8192i32.to_le_bytes());
        assert!(dib_to_png(&huge).unwrap_err().to_string().contains("too large"));
    }

    #[test]
    fn every_png_colour_type_becomes_the_same_bitmap() {
        let encode = |color: png::ColorType, data: &[u8]| {
            let mut out = Vec::new();
            let mut encoder = png::Encoder::new(&mut out, 2, 1);
            encoder.set_color(color);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header().unwrap().write_image_data(data).unwrap();
            out
        };
        let rgb = png_to_dib(&encode(png::ColorType::Rgb, &[10, 20, 30, 40, 50, 60])).unwrap();
        assert_eq!(&rgb[40..], &[30, 20, 10, 255, 60, 50, 40, 255]);
        let gray = png_to_dib(&encode(png::ColorType::GrayscaleAlpha, &[7, 128, 9, 255])).unwrap();
        assert_eq!(&gray[40..], &[7, 7, 7, 128, 9, 9, 9, 255]);
        assert_eq!(decode_png(&encode(png::ColorType::Grayscale, &[5, 6])).unwrap().2, [5, 5, 5, 255, 6, 6, 6, 255]);
    }
}
