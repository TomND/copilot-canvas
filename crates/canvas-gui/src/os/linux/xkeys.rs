//! Keys typed into a window through the X server: the latency bench's typing,
//! as Windows posts `WM_KEYDOWN` and macOS posts `NSEvent`s. The window takes
//! the input focus (winit reads keys of its focused window only) and gets a
//! `KeyPress` and a `KeyRelease` for the character's key, which winit turns
//! into text with the server's keymap. libX11 is loaded at run time; errors
//! go to winit's handler (Xlib's handler is per process), so a closed window
//! never ends the GUI.

use std::sync::Mutex;

use x11_dl::xlib;

struct Conn {
    xlib: xlib::Xlib,
    display: *mut xlib::Display,
}

// SAFETY: the display connection is used under the mutex only (winit made
// Xlib thread-safe with XInitThreads before its own connection).
unsafe impl Send for Conn {}

static CONN: Mutex<Option<Conn>> = Mutex::new(None);

/// The keysym for `c` and whether it needs Shift: ASCII letters, digits,
/// space, Enter and Backspace (what the benches type).
fn keysym(c: char) -> Option<(u64, bool)> {
    Some(match c {
        '\r' | '\n' => (0xff0d, false),
        '\u{8}' => (0xff08, false),
        c if c.is_ascii_uppercase() => (c.to_ascii_lowercase() as u64, true),
        c if c.is_ascii_lowercase() || c.is_ascii_digit() || c == ' ' => (c as u64, false),
        _ => return None,
    })
}

/// Sends `c` to the X11 window `window` (nothing for other characters or
/// without an X server).
pub fn post_key(window: u64, c: char) {
    let Some((sym, shift)) = keysym(c) else { return };
    let mut conn = CONN.lock().unwrap_or_else(|p| p.into_inner());
    if conn.is_none() {
        let Ok(xlib) = xlib::Xlib::open() else { return };
        // SAFETY: opens a connection to $DISPLAY; null when there is none.
        let display = unsafe { (xlib.XOpenDisplay)(std::ptr::null()) };
        if display.is_null() {
            return;
        }
        *conn = Some(Conn { xlib, display });
    }
    let Some(Conn { xlib: x, display }) = conn.as_ref() else { return };
    let display = *display;
    // SAFETY: Xlib calls on this connection, under the lock; the event is
    // zeroed before its key fields are set.
    unsafe {
        let keycode = (x.XKeysymToKeycode)(display, sym);
        if keycode == 0 {
            return;
        }
        (x.XSetInputFocus)(display, window, xlib::RevertToParent, xlib::CurrentTime);
        let root = (x.XDefaultRootWindow)(display);
        for (kind, mask) in [(xlib::KeyPress, xlib::KeyPressMask), (xlib::KeyRelease, xlib::KeyReleaseMask)] {
            let mut event: xlib::XEvent = std::mem::zeroed();
            event.key = xlib::XKeyEvent {
                type_: kind,
                serial: 0,
                send_event: xlib::True,
                display,
                window,
                root,
                subwindow: 0,
                time: xlib::CurrentTime,
                x: 1,
                y: 1,
                x_root: 1,
                y_root: 1,
                state: if shift { xlib::ShiftMask } else { 0 },
                keycode: keycode.into(),
                same_screen: xlib::True,
            };
            (x.XSendEvent)(display, window, xlib::True, mask, &mut event);
        }
        (x.XFlush)(display);
    }
}

#[cfg(test)]
mod tests {
    use super::keysym;

    #[test]
    fn keysyms_for_the_bench_keys() {
        assert_eq!(keysym('x'), Some((0x78, false)));
        assert_eq!(keysym('X'), Some((0x78, true)));
        assert_eq!(keysym('\r'), Some((0xff0d, false)));
        assert_eq!(keysym('7'), Some((0x37, false)));
        assert_eq!(keysym('é'), None);
    }
}
