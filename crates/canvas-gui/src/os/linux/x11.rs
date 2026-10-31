//! What the GUI asks the X server itself (winit has neither): the window
//! manager's work areas, and keys typed into a window for the latency bench
//! (as Windows posts `WM_KEYDOWN` and macOS posts `NSEvent`s). One Xlib
//! connection of our own, opened on first use; libX11 is loaded at run time
//! and errors go to winit's handler (Xlib's handler is per process), so a
//! closed window never ends the GUI.

use std::ffi::CString;
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

/// Runs `f` with the connection, opening it first (nothing without an X
/// server).
fn with<T>(f: impl FnOnce(&xlib::Xlib, *mut xlib::Display) -> Option<T>) -> Option<T> {
    let mut conn = CONN.lock().unwrap_or_else(|p| p.into_inner());
    if conn.is_none() {
        let xlib = xlib::Xlib::open().ok()?;
        // SAFETY: opens a connection to $DISPLAY; null when there is none.
        let display = unsafe { (xlib.XOpenDisplay)(std::ptr::null()) };
        if display.is_null() {
            return None;
        }
        *conn = Some(Conn { xlib, display });
    }
    let conn = conn.as_ref()?;
    f(&conn.xlib, conn.display)
}

/// The root window's CARDINAL property `name` (absent: `None`).
fn cardinals(x: &xlib::Xlib, display: *mut xlib::Display, name: &str) -> Option<Vec<i64>> {
    let name = CString::new(name).ok()?;
    // SAFETY: Xlib calls on a live connection; the property's data is read
    // within its length (format 32 is stored as C longs) and freed once.
    unsafe {
        let atom = (x.XInternAtom)(display, name.as_ptr(), xlib::True);
        if atom == 0 {
            return None;
        }
        let root = (x.XDefaultRootWindow)(display);
        let (mut kind, mut format, mut count, mut after, mut data) = (0, 0, 0, 0, std::ptr::null_mut());
        let status = (x.XGetWindowProperty)(display, root, atom, 0, 1024, xlib::False, xlib::XA_CARDINAL, &mut kind, &mut format, &mut count, &mut after, &mut data);
        if status != i32::from(xlib::Success) || data.is_null() {
            return None;
        }
        let values = (kind == xlib::XA_CARDINAL && format == 32).then(|| std::slice::from_raw_parts(data as *const std::ffi::c_long, count as usize).to_vec());
        (x.XFree)(data.cast());
        values
    }
}

/// The work area of the monitor at `monitor` (x0, y0, x1, y1 in root
/// pixels), as GTK finds it: Mutter's per-monitor `_GTK_WORKAREAS_D<n>`
/// when set, else the desktop's `_NET_WORKAREA` within the monitor. `None`
/// without a window manager that sets them (the whole monitor then).
pub fn work_area(monitor: [i32; 4]) -> Option<[i32; 4]> {
    with(|x, display| {
        let desktop = cardinals(x, display, "_NET_CURRENT_DESKTOP").and_then(|v| v.first().copied()).unwrap_or(0).max(0) as usize;
        let per_monitor = cardinals(x, display, &format!("_GTK_WORKAREAS_D{desktop}")).unwrap_or_default();
        let desktop_area = cardinals(x, display, "_NET_WORKAREA").and_then(|v| v.as_chunks::<4>().0.get(desktop).copied());
        pick(monitor, &per_monitor, desktop_area)
    })
}

/// The work area for `monitor` from the per-monitor areas (the one that
/// overlaps it most) or the desktop's single area (cut to the monitor).
fn pick(monitor: [i32; 4], per_monitor: &[i64], desktop: Option<[i64; 4]>) -> Option<[i32; 4]> {
    let rect = |v: &[i64; 4]| -> [i64; 4] { [v[0], v[1], v[0] + v[2], v[1] + v[3]] };
    let m = monitor.map(i64::from);
    let cut = |r: [i64; 4]| [r[0].max(m[0]), r[1].max(m[1]), r[2].min(m[2]), r[3].min(m[3])];
    let area = |r: [i64; 4]| (r[2] - r[0]).max(0) * (r[3] - r[1]).max(0);
    let best = per_monitor.as_chunks::<4>().0.iter().map(|v| cut(rect(v))).filter(|r| area(*r) > 0).max_by_key(|r| area(*r));
    let found = best.or_else(|| desktop.map(|v| cut(rect(&v))).filter(|r| area(*r) > 0))?;
    Some(found.map(|v| v as i32))
}

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

/// Types `c` into the X11 window `window`: it takes the input focus (winit
/// reads keys of its focused window only) and gets a `KeyPress` and a
/// `KeyRelease` for the character's key, which winit turns into text with
/// the server's keymap. Nothing for other characters or without an X server.
pub fn post_key(window: u64, c: char) {
    let Some((sym, shift)) = keysym(c) else { return };
    with(|x, display| {
        // SAFETY: Xlib calls on this connection, under the lock; the event is
        // zeroed before its key fields are set.
        unsafe {
            let keycode = (x.XKeysymToKeycode)(display, sym);
            if keycode == 0 {
                return None;
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
        Some(())
    });
}

#[cfg(test)]
mod tests {
    use super::{keysym, pick};

    #[test]
    fn keysyms_for_the_bench_keys() {
        assert_eq!(keysym('x'), Some((0x78, false)));
        assert_eq!(keysym('X'), Some((0x78, true)));
        assert_eq!(keysym('\r'), Some((0xff0d, false)));
        assert_eq!(keysym('7'), Some((0x37, false)));
        assert_eq!(keysym('é'), None);
    }

    #[test]
    fn work_areas_per_monitor_or_cut_from_the_desktop() {
        let left = [0, 0, 1920, 1080];
        let right = [1920, 0, 4480, 1440];
        // GNOME's top bar on the left monitor, nothing on the right one.
        let per_monitor = [0, 32, 1920, 1048, 1920, 0, 2560, 1440];
        assert_eq!(pick(left, &per_monitor, None), Some([0, 32, 1920, 1080]));
        assert_eq!(pick(right, &per_monitor, None), Some(right));
        // One desktop-wide area (a bottom panel on the left monitor only shrinks every monitor's bottom).
        let desktop = [0, 0, 4480, 1040];
        assert_eq!(pick(left, &[], Some(desktop)), Some([0, 0, 1920, 1040]));
        assert_eq!(pick(right, &[], Some(desktop)), Some([1920, 0, 4480, 1040]));
        // Nothing set, or nothing on this monitor: the whole monitor.
        assert_eq!(pick(left, &[], None), None);
        assert_eq!(pick([5000, 0, 6000, 900], &[], Some(desktop)), None);
    }
}
