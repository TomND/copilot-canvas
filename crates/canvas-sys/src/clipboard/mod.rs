//! The system clipboard of this machine: text, whether it holds an image,
//! the image as PNG, and (the service side of a remote image paste) setting
//! an image. Windows: the Win32 clipboard (unchanged code); macOS:
//! `NSPasteboard`; Linux: the `wl-clipboard` or `xclip`/`xsel` tools of the
//! session (the GUI uses its own display connection; MAC-LINUX-PLAN §5.6).

#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_image;
#[cfg(windows)]
pub use windows::{get_image_png, get_text, has_image, has_text, set_text};
#[cfg(windows)]
pub use windows_image::set_image_png;

#[cfg(macos)]
mod macos;
#[cfg(macos)]
pub use macos::{get_image_png, get_text, has_image, has_text, set_image_png, set_text};

#[cfg(linux)]
mod linux;
#[cfg(linux)]
pub use linux::{get_image_png, get_text, has_image, has_text, set_image_png, set_text};

#[cfg(all(test, desktop_unix))]
mod tests {
    /// Touches the user's real clipboard (restoring its text afterwards), so it
    /// runs only with `COPILOT_CANVAS_CLIPBOARD_TESTS=1`.
    #[test]
    fn round_trips_text_and_images() {
        if std::env::var_os("COPILOT_CANVAS_CLIPBOARD_TESTS").is_none() {
            eprintln!("skipped: set COPILOT_CANVAS_CLIPBOARD_TESTS=1 to use the real clipboard");
            return;
        }
        let previous = super::get_text().ok();
        let text = "copilot canvas ✓ 中文\nline 2";
        super::set_text(text).unwrap();
        assert!(super::has_text() && !super::has_image());
        assert_eq!(super::get_text().unwrap(), text);
        let png = canvas_common::clipimage::encode_png(2, 2, &[255; 16]).unwrap();
        super::set_image_png(&png).unwrap();
        assert!(super::has_image());
        assert!(super::get_image_png().unwrap().starts_with(b"\x89PNG"));
        if let Some(previous) = previous {
            let _ = super::set_text(&previous);
        }
    }
}
