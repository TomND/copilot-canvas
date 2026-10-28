//! What a front end provides from the machine it runs on: the clipboard,
//! opening links, bells, taskbar progress and the window title. The TUI's
//! implementation ([`TerminalPlatform`]) writes bells and progress to the
//! outer terminal; the GUI uses the Windows shell directly.

use anyhow::Result;

pub trait Platform: Send + Sync {
    fn set_clipboard_text(&self, text: &str) -> Result<()> {
        crate::clipboard::set_text(text)
    }

    fn clipboard_text(&self) -> Result<String> {
        crate::clipboard::get_text()
    }

    /// The clipboard holds text (an image or copied files do not).
    fn clipboard_has_text(&self) -> bool {
        crate::clipboard::has_text()
    }

    fn clipboard_has_image(&self) -> bool {
        crate::clipboard::has_image()
    }

    /// Opens a URL or path safely (never executes it); returns a status line.
    fn open_link(&self, target: &str) -> Result<String> {
        crate::links::open(target)
    }

    /// A session rang its bell or sent a notification. `out` is the byte
    /// stream for the outer terminal (written before the next frame).
    fn bell(&self, out: &mut Vec<u8>) {
        out.push(0x07);
    }

    /// Mirrors the canvas state to the taskbar: 0 none, 3 busy, 4 paused
    /// (needs you); `value` in percent.
    fn progress(&self, state: u8, value: u8, out: &mut Vec<u8>) {
        out.extend_from_slice(format!("\x1b]9;4;{state};{value}\x07").as_bytes());
    }

    /// The window title (`attention`: sessions that need you).
    fn title(&self, _attention: usize) {}

    /// A session's program sent a desktop notification (OSC 777 / OSC 9;
    /// GUI: a toast). The bell for it is rung separately.
    fn notification(&self, _session: &str, _title: &str, _body: &str) {}

    /// A session started to need you (it waits for an answer or a
    /// permission; GUI: a toast and a flashing taskbar button when the
    /// window is in the background).
    fn needs_you(&self, _session: &str) {}
}

/// The TUI: bells and OSC 9;4 progress go to the outer terminal.
pub struct TerminalPlatform;

impl Platform for TerminalPlatform {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_terminal_platform_writes_bells_and_progress_for_the_outer_terminal() {
        let mut out = Vec::new();
        TerminalPlatform.bell(&mut out);
        TerminalPlatform.progress(4, 100, &mut out);
        assert_eq!(out, b"\x07\x1b]9;4;4;100\x07");
    }
}
