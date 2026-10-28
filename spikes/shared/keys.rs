//! Keyboard → protocol input, shared by both spikes, plus OS-level key
//! injection (PostMessage to the window) for the latency measurement.

#![allow(dead_code)]

use canvas_common::protocol::{Input, Key, MOD_ALT, MOD_CTRL, MOD_SHIFT};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Named {
    Enter,
    Tab,
    Backspace,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

impl Mods {
    pub fn bits(self) -> u8 {
        (if self.shift { MOD_SHIFT } else { 0 }) | (if self.ctrl { MOD_CTRL } else { 0 }) | (if self.alt { MOD_ALT } else { 0 })
    }
}

/// GPUI key names ("enter", "f5", …) → named keys.
pub fn named_from_str(key: &str) -> Option<Named> {
    Some(match key {
        "enter" => Named::Enter,
        "tab" => Named::Tab,
        "backspace" => Named::Backspace,
        "escape" => Named::Esc,
        "up" => Named::Up,
        "down" => Named::Down,
        "left" => Named::Left,
        "right" => Named::Right,
        "home" => Named::Home,
        "end" => Named::End,
        "pageup" => Named::PageUp,
        "pagedown" => Named::PageDown,
        "insert" => Named::Insert,
        "delete" => Named::Delete,
        f if f.len() >= 2 && f.starts_with('f') && f[1..].chars().all(|c| c.is_ascii_digit()) => Named::F(f[1..].parse().ok()?),
        _ => return None,
    })
}

pub fn named_input(named: Named, mods: Mods) -> Input {
    let key = match named {
        Named::Enter => Key::Enter,
        Named::Tab if mods.shift => return Input::Key { key: Key::BackTab, mods: mods.bits() & !MOD_SHIFT },
        Named::Tab => Key::Tab,
        Named::Backspace => Key::Backspace,
        Named::Esc => Key::Esc,
        Named::Up => Key::Up,
        Named::Down => Key::Down,
        Named::Left => Key::Left,
        Named::Right => Key::Right,
        Named::Home => Key::Home,
        Named::End => Key::End,
        Named::PageUp => Key::PageUp,
        Named::PageDown => Key::PageDown,
        Named::Insert => Key::Insert,
        Named::Delete => Key::Delete,
        Named::F(n) => Key::F(n),
    };
    Input::Key { key, mods: mods.bits() }
}

/// Committed text (typing, IME commit, AltGr characters) → input: one
/// character is a key press, more is a paste.
pub fn text_input(text: &str) -> Option<Input> {
    let text: String = text.chars().filter(|c| !c.is_control()).collect();
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (None, _) => None,
        (Some(c), None) => Some(Input::Key { key: Key::Char(c), mods: 0 }),
        _ => Some(Input::Paste(text)),
    }
}

/// A key press with modifiers. `text` is what the layout produced (with all
/// modifiers); `base` is the key's character without modifiers.
///
/// AltGr arrives as Ctrl+Alt (or with both filtered out, as winit does on
/// AltGr layouts); Ctrl+Alt that produces printable text is treated as plain
/// text, so `@` on a German keyboard reaches the terminal as `@`.
pub fn key_input(named: Option<Named>, text: Option<&str>, base: Option<char>, mods: Mods) -> Option<Input> {
    if let Some(named) = named {
        return Some(named_input(named, mods));
    }
    let printable = text.filter(|t| !t.is_empty() && !t.chars().any(char::is_control));
    if mods.ctrl && mods.alt && printable.is_some() {
        return text_input(printable?);
    }
    if mods.ctrl || mods.alt {
        let c = base.or_else(|| printable.and_then(|t| t.chars().next()))?;
        let c = if mods.ctrl { c.to_ascii_lowercase() } else { printable.and_then(|t| t.chars().next()).unwrap_or(c) };
        let bits = Mods { shift: mods.shift && mods.ctrl, ..mods }.bits();
        return Some(Input::Key { key: Key::Char(c), mods: bits });
    }
    text_input(printable?)
}

/// Posts WM_KEYDOWN (and WM_KEYUP 20 ms later) for an ASCII letter or digit to
/// a window, as the OS would deliver a real key press (minus the hardware input
/// queue). The key-up is delayed so the WM_CHAR that TranslateMessage queues
/// for the key-down comes first, as with real input (winit relies on that).
pub fn post_key(hwnd: isize, c: char) -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{MAPVK_VK_TO_VSC, MapVirtualKeyW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_KEYDOWN, WM_KEYUP};
    let vk = c.to_ascii_uppercase() as u32;
    let scan = unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_VSC) };
    let down = 1isize | ((scan as isize) << 16);
    let up = down | (1 << 30) | (1 << 31);
    let ok = unsafe { PostMessageW(hwnd as _, WM_KEYDOWN, vk as usize, down) != 0 };
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        unsafe { PostMessageW(hwnd as _, WM_KEYUP, vk as usize, up) };
    });
    ok
}

/// Posts WM_CHAR, as TranslateMessage does after a key press that produced a
/// character (used to exercise the AltGr text path without a German layout).
pub fn post_char(hwnd: isize, c: char) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CHAR};
    let mut buf = [0u16; 2];
    c.encode_utf16(&mut buf).iter().all(|unit| unsafe { PostMessageW(hwnd as _, WM_CHAR, *unit as usize, 1) != 0 })
}

/// Simulates the message sequence of AltGr+`key` producing `c` on a layout
/// with AltGr: this thread's key state shows Ctrl + Alt + right Alt held, then
/// WM_KEYDOWN for the key followed by WM_CHAR(c). Call `release_altgr` once
/// the window has processed them. (The US layout itself has no AltGr, so
/// TranslateMessage adds no character of its own.)
pub fn post_altgr(hwnd: isize, key: char, c: char) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyboardState, MAPVK_VK_TO_VSC, MapVirtualKeyW, SetKeyboardState, VK_CONTROL, VK_LCONTROL, VK_MENU, VK_RMENU,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_KEYDOWN};
    let mut state = [0u8; 256];
    unsafe { GetKeyboardState(state.as_mut_ptr()) };
    for vk in [VK_CONTROL, VK_LCONTROL, VK_MENU, VK_RMENU] {
        state[vk as usize] = 0x80;
    }
    unsafe { SetKeyboardState(state.as_ptr()) };
    let vk = key.to_ascii_uppercase() as u32;
    let scan = unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_VSC) };
    unsafe { PostMessageW(hwnd as _, WM_KEYDOWN, vk as usize, 1 | ((scan as isize) << 16)) };
    post_char(hwnd, c);
}

pub fn release_altgr() {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardState, SetKeyboardState, VK_CONTROL, VK_LCONTROL, VK_MENU, VK_RMENU};
    let mut state = [0u8; 256];
    unsafe { GetKeyboardState(state.as_mut_ptr()) };
    for vk in [VK_CONTROL, VK_LCONTROL, VK_MENU, VK_RMENU] {
        state[vk as usize] = 0;
    }
    unsafe { SetKeyboardState(state.as_ptr()) };
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Mods = Mods { shift: false, ctrl: false, alt: false };

    #[test]
    fn altgr_text_is_plain_text() {
        let altgr = Mods { ctrl: true, alt: true, shift: false };
        assert_eq!(key_input(None, Some("@"), Some('q'), altgr), Some(Input::Key { key: Key::Char('@'), mods: 0 }));
        assert_eq!(key_input(None, Some("€"), Some('e'), altgr), Some(Input::Key { key: Key::Char('€'), mods: 0 }));
        // winit filters Ctrl and Alt out on AltGr layouts.
        assert_eq!(key_input(None, Some("{"), Some('7'), NONE), Some(Input::Key { key: Key::Char('{'), mods: 0 }));
    }

    #[test]
    fn ctrl_and_alt_chords_keep_their_modifiers() {
        let ctrl = Mods { ctrl: true, ..NONE };
        let alt = Mods { alt: true, ..NONE };
        assert_eq!(key_input(None, Some("\u{3}"), Some('c'), ctrl), Some(Input::Key { key: Key::Char('c'), mods: MOD_CTRL }));
        assert_eq!(key_input(None, None, Some('C'), ctrl), Some(Input::Key { key: Key::Char('c'), mods: MOD_CTRL }));
        assert_eq!(key_input(None, Some("b"), Some('b'), alt), Some(Input::Key { key: Key::Char('b'), mods: MOD_ALT }));
        // Ctrl+Alt without text (US layout) stays a chord.
        let both = Mods { ctrl: true, alt: true, ..NONE };
        assert_eq!(key_input(None, None, Some('x'), both), Some(Input::Key { key: Key::Char('x'), mods: MOD_CTRL | MOD_ALT }));
    }

    #[test]
    fn named_keys_and_text() {
        let shift = Mods { shift: true, ..NONE };
        assert_eq!(key_input(Some(Named::Tab), None, None, shift), Some(Input::Key { key: Key::BackTab, mods: 0 }));
        assert_eq!(key_input(Some(Named::Enter), Some("\r"), None, shift), Some(Input::Key { key: Key::Enter, mods: MOD_SHIFT }));
        assert_eq!(named_from_str("f12"), Some(Named::F(12)));
        assert_eq!(text_input("日本語"), Some(Input::Paste("日本語".into())));
        assert_eq!(key_input(None, Some("A"), Some('a'), shift), Some(Input::Key { key: Key::Char('A'), mods: 0 }));
    }
}
