//! Windows clipboard (Unicode text, images read as PNG).

use anyhow::{Result, bail};
use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard, SetClipboardData};
use windows_sys::Win32::Foundation::GlobalFree;
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

const CF_UNICODETEXT: u32 = 13;

/// Another app may hold the clipboard briefly; retry for ~250 ms.
fn open() -> Result<()> {
    for _ in 0..25 {
        // SAFETY: no owner window; paired with CloseClipboard.
        if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    bail!("the clipboard is busy")
}

pub fn set_text(text: &str) -> Result<()> {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    open()?;
    // SAFETY: the clipboard is open; on success the system owns `memory`.
    let result = unsafe {
        EmptyClipboard();
        let bytes = wide.len() * 2;
        let memory = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if memory.is_null() {
            CloseClipboard();
            bail!("out of memory for the clipboard");
        }
        let target = GlobalLock(memory) as *mut u16;
        std::ptr::copy_nonoverlapping(wide.as_ptr(), target, wide.len());
        GlobalUnlock(memory);
        let ok = !SetClipboardData(CF_UNICODETEXT, memory).is_null();
        if !ok {
            GlobalFree(memory);
        }
        CloseClipboard();
        ok
    };
    if !result {
        bail!("could not write to the clipboard");
    }
    Ok(())
}

/// True when the clipboard holds text (an image or copied files do not).
pub fn has_text() -> bool {
    // SAFETY: no clipboard ownership needed for this query.
    unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) != 0 }
}

/// True when the clipboard holds a bitmap (a screenshot, a copied image).
pub fn has_image() -> bool {
    const CF_BITMAP: u32 = 2;
    const CF_DIB: u32 = 8;
    const CF_DIBV5: u32 = 17;
    // SAFETY: no clipboard ownership needed for these queries.
    [CF_DIB, CF_DIBV5, CF_BITMAP].into_iter().any(|format| unsafe { IsClipboardFormatAvailable(format) != 0 })
}

pub fn get_text() -> Result<String> {
    open()?;
    // SAFETY: the clipboard is open; the handle stays valid until it closes.
    let text = unsafe {
        let handle = GetClipboardData(CF_UNICODETEXT);
        let text = if handle.is_null() {
            None
        } else {
            let data = GlobalLock(handle) as *const u16;
            let text = if data.is_null() {
                None
            } else {
                let mut len = 0;
                while *data.add(len) != 0 {
                    len += 1;
                }
                Some(String::from_utf16_lossy(std::slice::from_raw_parts(data, len)))
            };
            GlobalUnlock(handle);
            text
        };
        CloseClipboard();
        text
    };
    text.ok_or_else(|| anyhow::anyhow!("the clipboard has no text"))
}

/// The clipboard's image as PNG: the "PNG" format when a program offers it
/// (keeps transparency), else the bitmap (`CF_DIBV5`, `CF_DIB`) converted
/// straight from the clipboard's memory (no copy of the bitmap). At most
/// `clipimage::MAX_PNG` bytes; a larger PNG is refused before it is copied.
pub fn get_image_png() -> Result<Vec<u8>> {
    use canvas_common::clipimage;
    use windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW;
    use windows_sys::Win32::System::Memory::GlobalSize;
    const CF_DIB: u32 = 8;
    const CF_DIBV5: u32 = 17;
    let name: Vec<u16> = "PNG".encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: a null-terminated format name.
    let png_format = unsafe { RegisterClipboardFormatW(name.as_ptr()) };
    open()?;
    // SAFETY: the clipboard is open; each handle stays valid until it closes.
    let png = unsafe {
        let mut found = None;
        for format in [png_format, CF_DIBV5, CF_DIB] {
            if format == 0 || IsClipboardFormatAvailable(format) == 0 {
                continue;
            }
            let handle = GetClipboardData(format);
            if handle.is_null() {
                continue;
            }
            let data = GlobalLock(handle) as *const u8;
            if data.is_null() {
                continue;
            }
            let len = GlobalSize(handle);
            let bytes = std::slice::from_raw_parts(data, len);
            let converted = if format == png_format {
                if len <= clipimage::MAX_PNG { Ok(bytes.to_vec()) } else { Err(anyhow::anyhow!("the image is larger than {} MB", clipimage::MAX_PNG >> 20)) }
            } else {
                clipimage::dib_to_png(bytes)
            };
            GlobalUnlock(handle);
            found = Some(converted);
            break;
        }
        CloseClipboard();
        found.ok_or_else(|| anyhow::anyhow!("the clipboard has no image"))??
    };
    anyhow::ensure!(png.len() <= clipimage::MAX_PNG, "the image is larger than {} MB", clipimage::MAX_PNG >> 20);
    Ok(png)
}

#[cfg(test)]
mod tests {
    /// The clipboard is unavailable while the workstation is locked (or from a
    /// non-interactive desktop): OpenClipboard fails with ERROR_ACCESS_DENIED.
    fn clipboard_unavailable() -> bool {
        // SAFETY: paired with CloseClipboard on success.
        if unsafe { super::OpenClipboard(std::ptr::null_mut()) } != 0 {
            unsafe { super::CloseClipboard() };
            return false;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(5)
    }

    #[test]
    fn round_trips_unicode_text() {
        if clipboard_unavailable() {
            eprintln!("skipped: the clipboard is not accessible (locked workstation or non-interactive desktop)");
            return;
        }
        let previous = super::get_text().ok();
        let text = "copilot canvas ✓ 中文\r\nline 2";
        super::set_text(text).unwrap();
        assert_eq!(super::get_text().unwrap(), text);
        if let Some(previous) = previous {
            let _ = super::set_text(&previous);
        }
    }
}
