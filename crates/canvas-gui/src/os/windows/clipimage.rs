//! The clipboard's image, for the preview shown when you paste a screenshot
//! into a terminal (the agent attaches the image itself). Reads the "PNG"
//! format when present, else a device-independent bitmap (`CF_DIBV5` or
//! `CF_DIB`, 24 or 32 bits per pixel).

use std::time::Duration;

const CF_DIB: u32 = 8;
const CF_DIBV5: u32 = 17;

/// The clipboard's image as RGBA rows (`None`: no image, or it cannot be read).
pub fn read() -> Option<(u32, u32, Vec<u8>)> {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW};
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    // Another program may hold the clipboard for a moment.
    // SAFETY: plain Win32 calls; the clipboard is closed on every path below.
    let opened = (0..10).any(|_| unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 || {
        std::thread::sleep(Duration::from_millis(10));
        false
    });
    if !opened {
        return None;
    }
    let png_name: Vec<u16> = "PNG".encode_utf16().chain(Some(0)).collect();
    // SAFETY: a NUL-terminated name.
    let png_format = unsafe { RegisterClipboardFormatW(png_name.as_ptr()) };
    let mut result = None;
    for (format, is_png) in [(png_format, true), (CF_DIBV5, false), (CF_DIB, false)] {
        // SAFETY: the clipboard is open.
        if format == 0 || unsafe { IsClipboardFormatAvailable(format) } == 0 {
            continue;
        }
        // SAFETY: the clipboard is open; the handle stays owned by the clipboard.
        let handle = unsafe { GetClipboardData(format) };
        if handle.is_null() {
            continue;
        }
        // SAFETY: a global memory handle from the clipboard, locked while read.
        let bytes = unsafe {
            let ptr = GlobalLock(handle);
            if ptr.is_null() {
                continue;
            }
            let bytes = std::slice::from_raw_parts(ptr as *const u8, GlobalSize(handle)).to_vec();
            GlobalUnlock(handle);
            bytes
        };
        result = if is_png { crate::images::decode_png(&bytes) } else { decode_dib(&bytes) };
        if result.is_some() {
            break;
        }
    }
    // SAFETY: opened above.
    unsafe { CloseClipboard() };
    result
}

/// A packed DIB (BITMAPINFOHEADER or a later header, then pixels) as RGBA.
pub fn decode_dib(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let u32_at = |at: usize| bytes.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let header = u32_at(0)? as usize;
    let width = u32_at(4)? as i32;
    let height = u32_at(8)? as i32;
    let bits = u16::from_le_bytes([*bytes.get(14)?, *bytes.get(15)?]);
    let compression = u32_at(16)?;
    if width <= 0 || height == 0 || !(bits == 24 || bits == 32) || !(compression == 0 || compression == 3) || header < 40 {
        return None;
    }
    let (width, rows) = (width as u32, height.unsigned_abs());
    if width as u64 * rows as u64 > 4096 * 4096 {
        return None;
    }
    // BI_BITFIELDS with a plain 40-byte header: three masks follow it.
    let masks = if compression == 3 && header == 40 { 12 } else { 0 };
    let offset = header + masks;
    let stride = (width as usize * bits as usize / 8).div_ceil(4) * 4;
    let pixels = bytes.get(offset..offset + stride * rows as usize)?;
    let mut rgba = Vec::with_capacity(width as usize * rows as usize * 4);
    let mut any_alpha = false;
    for y in 0..rows as usize {
        // Positive heights are stored bottom-up.
        let row = if height > 0 { rows as usize - 1 - y } else { y };
        let line = &pixels[row * stride..row * stride + stride];
        for x in 0..width as usize {
            let px = &line[x * bits as usize / 8..];
            let alpha = if bits == 32 { px[3] } else { 255 };
            any_alpha |= bits == 32 && alpha != 0;
            rgba.extend_from_slice(&[px[2], px[1], px[0], alpha]);
        }
    }
    // Many programs leave the alpha of 32-bit bitmaps at zero: show them opaque.
    if bits == 32 && !any_alpha {
        rgba.as_chunks_mut::<4>().0.iter_mut().for_each(|p| p[3] = 255);
    }
    Some((width, rows, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dib(width: i32, height: i32, bits: u16, pixels: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&40u32.to_le_bytes());
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(&[0; 24]);
        out.extend_from_slice(pixels);
        out
    }

    #[test]
    fn bitmaps_become_rgba_rows_top_down() {
        // 2 × 2, 24 bits, bottom-up: bottom row blue, red; top row green, white (rows padded to 8 bytes).
        let pixels = [255, 0, 0, 0, 0, 255, 0, 0, 0, 255, 0, 255, 255, 255, 0, 0];
        let (w, h, rgba) = decode_dib(&dib(2, 2, 24, &pixels)).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(&rgba[..8], &[0, 255, 0, 255, 255, 255, 255, 255], "top row first");
        assert_eq!(&rgba[8..], &[0, 0, 255, 255, 255, 0, 0, 255]);
        // 32 bits with zero alpha everywhere: opaque.
        let (_, _, rgba) = decode_dib(&dib(1, -1, 32, &[10, 20, 30, 0])).unwrap();
        assert_eq!(rgba, [30, 20, 10, 255]);
        assert!(decode_dib(&dib(1, 1, 8, &[0; 4])).is_none(), "palettes are not read");
    }
}
