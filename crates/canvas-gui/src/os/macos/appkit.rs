//! Small AppKit calls: keys posted to the application's event queue (they
//! go through AppKit and winit like real key presses: the GUI benches'
//! keystroke-to-present measurement) and screens' visible frames (monitor
//! work areas without the menu bar and the Dock).

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::msg_send;
use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
use objc2_foundation::{NSPoint, NSProcessInfo, NSRect, NSString};

/// The virtual key code (`kVK_*`) of an ASCII character the benches type.
fn key_code(c: char) -> u16 {
    const LETTERS: [u16; 26] = [0x00, 0x0B, 0x08, 0x02, 0x0E, 0x03, 0x05, 0x04, 0x22, 0x26, 0x28, 0x25, 0x2E, 0x2D, 0x1F, 0x23, 0x0C, 0x0F, 0x01, 0x11, 0x20, 0x09, 0x0D, 0x07, 0x10, 0x06];
    const DIGITS: [u16; 10] = [0x1D, 0x12, 0x13, 0x14, 0x15, 0x17, 0x16, 0x1A, 0x1C, 0x19];
    match c {
        '\r' | '\n' => 0x24,
        '\u{8}' => 0x33,
        ' ' => 0x31,
        'a'..='z' => LETTERS[c as usize - 'a' as usize],
        'A'..='Z' => LETTERS[c as usize - 'A' as usize],
        '0'..='9' => DIGITS[c as usize - '0' as usize],
        _ => 0x2C,
    }
}

/// Posts a key press and release for `c` to the window whose content view is `view`.
pub fn post_key(view: isize, c: char) {
    if view == 0 {
        return;
    }
    // SAFETY: `view` is the live NSView of one of this process's windows; the
    // events are created and posted as AppKit documents (posting is allowed
    // from any thread).
    unsafe {
        let view = view as *mut AnyObject;
        let window: *mut AnyObject = msg_send![view, window];
        if window.is_null() {
            return;
        }
        let number: isize = msg_send![window, windowNumber];
        let text = NSString::from_str(&c.to_string());
        let code = key_code(c);
        let Some(app_class) = AnyClass::get("NSApplication") else { return };
        let app: *mut AnyObject = msg_send![app_class, sharedApplication];
        for kind in [NSEventType::KeyDown, NSEventType::KeyUp] {
            let time = NSProcessInfo::processInfo().systemUptime();
            let event: Option<Retained<NSEvent>> = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                kind,
                NSPoint::new(0.0, 0.0),
                NSEventModifierFlags::empty(),
                time,
                number,
                None,
                &text,
                &text,
                false,
                code,
            );
            if let Some(event) = event {
                let _: () = msg_send![app, postEvent: &*event, atStart: false];
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

/// The screen's visible frame (no menu bar, no Dock) in physical pixels,
/// top-left origin, from winit's monitor (`[left, top, right, bottom]`).
pub fn work_area(monitor: &winit::monitor::MonitorHandle) -> Option<[i32; 4]> {
    use winit::platform::macos::MonitorHandleExtMacOS;
    let screen = monitor.ns_screen()? as *mut AnyObject;
    // SAFETY: winit's live NSScreen; plain property reads on the main thread (the event loop's).
    let (frame, visible, scale) = unsafe {
        let frame: NSRect = msg_send![screen, frame];
        let visible: NSRect = msg_send![screen, visibleFrame];
        let scale: f64 = msg_send![screen, backingScaleFactor];
        (frame, visible, scale)
    };
    let (pos, size) = (monitor.position(), monitor.size());
    let px = |points: f64| (points * scale).round() as i32;
    let left = px(visible.origin.x - frame.origin.x);
    let right = px((frame.origin.x + frame.size.width) - (visible.origin.x + visible.size.width));
    let top = px((frame.origin.y + frame.size.height) - (visible.origin.y + visible.size.height));
    let bottom = px(visible.origin.y - frame.origin.y);
    Some([pos.x + left, pos.y + top, pos.x + size.width as i32 - right, pos.y + size.height as i32 - bottom])
}

