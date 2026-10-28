//! The clipboard of the machine the backend runs on. A remote client's image
//! paste puts the image here (`Request::SetClipboardImage`) before the paste
//! key reaches the agent, which then reads this clipboard.

use anyhow::{Context, Result, bail, ensure};
use canvas_common::clipimage;
use windows_sys::Win32::Foundation::{GlobalFree, HANDLE};
use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

const CF_DIB: u32 = 8;

/// Another program may hold the clipboard briefly; retry for about a second.
fn open() -> Result<()> {
    for _ in 0..50 {
        // SAFETY: no owner window; paired with CloseClipboard.
        if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    bail!("the clipboard is busy or not available (a locked or disconnected session?)")
}

/// Copies `bytes` into movable global memory for the clipboard.
///
/// # Safety
/// The clipboard must be open; on success the system owns the memory.
unsafe fn put(format: u32, bytes: &[u8]) -> bool {
    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1));
        if memory.is_null() {
            return false;
        }
        let target = GlobalLock(memory) as *mut u8;
        if target.is_null() {
            GlobalFree(memory);
            return false;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), target, bytes.len());
        GlobalUnlock(memory);
        if SetClipboardData(format, memory as HANDLE).is_null() {
            GlobalFree(memory);
            return false;
        }
        true
    }
}

/// Replaces the clipboard with an image: a 32-bit `CF_DIB` (what most programs
/// read) and the registered "PNG" format (keeps transparency).
pub fn set_image_png(png: &[u8]) -> Result<()> {
    ensure!(png.len() <= clipimage::MAX_PNG, "the image is larger than {} MB", clipimage::MAX_PNG >> 20);
    let dib = clipimage::png_to_dib(png).context("cannot read the pasted image")?;
    let name: Vec<u16> = "PNG".encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: a null-terminated format name.
    let png_format = unsafe { RegisterClipboardFormatW(name.as_ptr()) };
    open()?;
    // SAFETY: the clipboard is open until CloseClipboard below.
    let ok = unsafe {
        EmptyClipboard();
        let ok = put(CF_DIB, &dib);
        if ok && png_format != 0 {
            put(png_format, png);
        }
        CloseClipboard();
        ok
    };
    ensure!(ok, "could not put the image on the clipboard");
    Ok(())
}
