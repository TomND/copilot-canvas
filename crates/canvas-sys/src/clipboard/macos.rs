//! macOS: the general `NSPasteboard`. Images are offered as PNG or TIFF
//! (screenshots, Preview); TIFF is converted to PNG with `NSBitmapImageRep`.

use anyhow::{Result, bail, ensure};
use canvas_common::clipimage;
use objc2::rc::{Retained, autoreleasepool};
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardType, NSPasteboardTypePNG, NSPasteboardTypeString, NSPasteboardTypeTIFF};
use objc2_foundation::{NSData, NSDictionary, NSString};

fn board() -> Retained<NSPasteboard> {
    // SAFETY: the general pasteboard may be used from any thread.
    unsafe { NSPasteboard::generalPasteboard() }
}

fn offers(kind: &NSPasteboardType) -> bool {
    // SAFETY: plain pasteboard queries.
    autoreleasepool(|_| unsafe { board().types() }.is_some_and(|types| types.iter().any(|t| t == kind)))
}

pub fn set_text(text: &str) -> Result<()> {
    let ok = autoreleasepool(|_| {
        let board = board();
        // SAFETY: clearing and writing the general pasteboard.
        unsafe {
            board.clearContents();
            board.setString_forType(&NSString::from_str(text), NSPasteboardTypeString)
        }
    });
    ensure!(ok, "could not write to the clipboard");
    Ok(())
}

/// True when the clipboard holds text (an image or copied files do not).
pub fn has_text() -> bool {
    // SAFETY: a static pasteboard type.
    offers(unsafe { NSPasteboardTypeString })
}

/// True when the clipboard holds an image (a screenshot, a copied image).
pub fn has_image() -> bool {
    // SAFETY: static pasteboard types.
    unsafe { offers(NSPasteboardTypePNG) || offers(NSPasteboardTypeTIFF) }
}

pub fn get_text() -> Result<String> {
    // SAFETY: reading the general pasteboard.
    autoreleasepool(|_| unsafe { board().stringForType(NSPasteboardTypeString) }.map(|text| text.to_string())).ok_or_else(|| anyhow::anyhow!("the clipboard has no text"))
}

/// The clipboard's image as PNG (PNG as offered, else TIFF converted), at most
/// `clipimage::MAX_PNG` bytes.
pub fn get_image_png() -> Result<Vec<u8>> {
    let png = autoreleasepool(|_| -> Result<Vec<u8>> {
        let board = board();
        // SAFETY: reading the general pasteboard; the image rep reads the data it is given.
        unsafe {
            if let Some(data) = board.dataForType(NSPasteboardTypePNG) {
                ensure!(data.len() <= clipimage::MAX_PNG, "the image is larger than {} MB", clipimage::MAX_PNG >> 20);
                return Ok(data.bytes().to_vec());
            }
            let Some(tiff) = board.dataForType(NSPasteboardTypeTIFF) else { bail!("the clipboard has no image") };
            let Some(rep) = NSBitmapImageRep::imageRepWithData(&tiff) else { bail!("cannot read the clipboard's image") };
            let png = rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()).ok_or_else(|| anyhow::anyhow!("cannot convert the clipboard's image"))?;
            Ok(png.bytes().to_vec())
        }
    })?;
    ensure!(png.len() <= clipimage::MAX_PNG, "the image is larger than {} MB", clipimage::MAX_PNG >> 20);
    Ok(png)
}

/// Replaces the clipboard with an image (PNG, and TIFF for programs that only read that).
pub fn set_image_png(png: &[u8]) -> Result<()> {
    ensure!(png.len() <= clipimage::MAX_PNG, "the image is larger than {} MB", clipimage::MAX_PNG >> 20);
    let ok = autoreleasepool(|_| {
        let board = board();
        let data = NSData::with_bytes(png);
        // SAFETY: clearing and writing the general pasteboard.
        unsafe {
            board.clearContents();
            let ok = board.setData_forType(Some(&data), NSPasteboardTypePNG);
            if ok && let Some(rep) = NSBitmapImageRep::imageRepWithData(&data) && let Some(tiff) = rep.TIFFRepresentation() {
                board.setData_forType(Some(&tiff), NSPasteboardTypeTIFF);
            }
            ok
        }
    });
    ensure!(ok, "could not put the image on the clipboard");
    Ok(())
}
