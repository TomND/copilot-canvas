//! `copilot-canvas kitty-demo` (hidden): prints a small image for the GUI's
//! inline-image check. The image is 32 × 16 pixels, magenta on the left half
//! and green on the right, shown over `DEMO_COLS` × `DEMO_ROWS` cells
//! between two marker lines. It is sent as an iTerm2 inline image (an OSC,
//! which ConPTY passes through; the host turns it into a Kitty placement);
//! `kitty-demo apc` sends the Kitty graphics command itself.

use std::io::Write;

use anyhow::Result;

/// Cells the image covers.
pub const DEMO_COLS: u16 = 8;
pub const DEMO_ROWS: u16 = 2;
/// The image's colours (left half, right half).
pub const LEFT: [u8; 3] = [255, 0, 255];
pub const RIGHT: [u8; 3] = [0, 255, 0];
/// Printed on the line before and after the image.
pub const BEGIN: &str = "KITTY_DEMO_BEGIN";
pub const END: &str = "KITTY_DEMO_END";

/// The demo image as a PNG.
pub fn png() -> Result<Vec<u8>> {
    let (width, height) = (32u32, 16u32);
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for _ in 0..height {
        for x in 0..width {
            let [r, g, b] = if x < width / 2 { LEFT } else { RIGHT };
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&rgba)?;
    writer.finish()?;
    Ok(out)
}

/// The escape sequences that transmit and place the image (`a=T`, PNG,
/// responses suppressed), split into chunks of at most 4096 base64 bytes.
pub fn escape(id: u32) -> Result<String> {
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD.encode(png()?);
    let chunks: Vec<&str> = data.as_bytes().chunks(4096).map(|c| std::str::from_utf8(c).unwrap_or_default()).collect();
    let mut out = String::new();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        if i == 0 {
            out.push_str(&format!("\x1b_Gf=100,a=T,i={id},c={DEMO_COLS},r={DEMO_ROWS},q=2,m={more};{chunk}\x1b\\"));
        } else {
            out.push_str(&format!("\x1b_Gm={more};{chunk}\x1b\\"));
        }
    }
    Ok(out)
}

/// The same image as an iTerm2 inline image over `DEMO_COLS` × `DEMO_ROWS` cells.
pub fn iterm() -> Result<String> {
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD.encode(png()?);
    Ok(format!("\x1b]1337;File=inline=1;width={DEMO_COLS};height={DEMO_ROWS};preserveAspectRatio=0:{data}\x07"))
}

/// Prints the markers and the image. The default form is an iTerm2 inline
/// image (an OSC, which ConPTY passes through; the host turns it into a Kitty
/// placement); `apc` prints the Kitty graphics command itself (dropped by
/// Windows' inbox ConPTY, shown by hosts that pass APC through).
pub fn run(mode: Option<&str>) -> Result<()> {
    enable_vt();
    let mut out = std::io::stdout().lock();
    write!(out, "{BEGIN}\r\n")?;
    match mode {
        Some("apc") => write!(out, "{}", escape(4242)?)?,
        _ => write!(out, "{}", iterm()?)?,
    }
    // ConPTY does not know the image's size: leave its rows free, then go on below.
    write!(out, "{}{END}\r\n", "\r\n".repeat(DEMO_ROWS as usize))?;
    out.flush()?;
    Ok(())
}

/// Lets the console pass escape sequences through to the pseudoconsole.
fn enable_vt() {
    use windows_sys::Win32::System::Console::{ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_OUTPUT_HANDLE, SetConsoleMode};
    // SAFETY: the process's own standard output handle; failures leave the mode as it was.
    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(handle, &mut mode) != 0 {
            SetConsoleMode(handle, mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_escape_carries_a_decodable_png() {
        let text = escape(7).unwrap();
        assert!(text.starts_with("\x1b_Gf=100,a=T,i=7,c=8,r=2,q=2,m=0;"));
        assert!(text.ends_with("\x1b\\"));
        let body = &text[text.find(';').unwrap() + 1..text.len() - 2];
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD.decode(body).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let reader = decoder.read_info().unwrap();
        assert_eq!((reader.info().width, reader.info().height), (32, 16));
    }
}
