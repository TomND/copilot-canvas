//! Platform-neutral input events. Every front end (the TUI's console reader,
//! a future GPU window) converts its native events into these and feeds them
//! to `App::on_input`, the single dispatch for keys, mouse, wheel and paste.
//!
//! The shapes mirror the console library the TUI used before (same variant
//! and field names), so key bindings read the same in both front ends.

use std::fmt;
use std::ops::{BitAnd, BitOr, BitOrAssign, Not, Sub};

use crate::protocol::{Key, MOD_ALT, MOD_CTRL, MOD_SHIFT};

#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    /// Text pasted at once: bracketed paste, or a burst of keys coalesced by
    /// [`coalesce`].
    Paste(String),
    /// The window gained (`true`) or lost (`false`) keyboard focus.
    Focus(bool),
    /// The surface is now this many cells (columns, rows).
    Resize(u16, u16),
}

/// Keyboard modifiers held during a key or mouse event.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Modifiers(u8);

impl Modifiers {
    pub const NONE: Self = Self(0);
    pub const SHIFT: Self = Self(1);
    pub const CONTROL: Self = Self(1 << 1);
    pub const ALT: Self = Self(1 << 2);
    /// The Windows / Command key.
    pub const SUPER: Self = Self(1 << 3);
    const ALL: Self = Self(0b1111);
    const NAMES: [(Self, &'static str); 4] = [(Self::SHIFT, "SHIFT"), (Self::CONTROL, "CONTROL"), (Self::ALT, "ALT"), (Self::SUPER, "SUPER")];

    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Unknown bits are dropped.
    pub const fn from_bits_truncate(bits: u8) -> Self {
        Self(bits & Self::ALL.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// All of `other` is held.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Any of `other` is held.
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }
}

impl BitOr for Modifiers {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

impl BitOrAssign for Modifiers {
    fn bitor_assign(&mut self, rhs: Self) {
        self.insert(rhs);
    }
}

impl BitAnd for Modifiers {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

impl Sub for Modifiers {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        self.difference(rhs)
    }
}

impl Not for Modifiers {
    type Output = Self;
    fn not(self) -> Self {
        Self(!self.0 & Self::ALL.0)
    }
}

impl fmt::Debug for Modifiers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Modifiers(")?;
        if self.is_empty() {
            f.write_str("NONE")?;
        }
        let mut first = true;
        for (flag, name) in Self::NAMES {
            if self.contains(flag) {
                if !first {
                    f.write_str(" | ")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        f.write_str(")")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyCode {
    /// The text the key produced, already reflecting Shift, Caps Lock and the
    /// keyboard layout (including AltGr compositions).
    Char(char),
    Enter,
    Tab,
    /// Shift+Tab.
    BackTab,
    Backspace,
    Esc,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// Function key F1..F24.
    F(u8),
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyEventKind {
    Press,
    Repeat,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyEvent {
    pub code: KeyCode,
    pub modifiers: Modifiers,
    pub kind: KeyEventKind,
}

impl KeyEvent {
    /// A key press.
    pub const fn new(code: KeyCode, modifiers: Modifiers) -> Self {
        Self { code, modifiers, kind: KeyEventKind::Press }
    }

    pub const fn with_kind(code: KeyCode, modifiers: Modifiers, kind: KeyEventKind) -> Self {
        Self { code, modifiers, kind }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseEventKind {
    Down(MouseButton),
    Up(MouseButton),
    /// The pointer moved while this button was held.
    Drag(MouseButton),
    /// The pointer moved with no button held.
    Moved,
    /// Wheel away from the user.
    ScrollUp,
    /// Wheel towards the user.
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MouseEvent {
    pub kind: MouseEventKind,
    /// The cell under the pointer.
    pub column: u16,
    pub row: u16,
    /// The precise pointer position in cell units (`column <= x < column + 1`).
    /// Cell-based front ends report the cell's origin.
    pub x: f32,
    pub y: f32,
    pub modifiers: Modifiers,
}

impl MouseEvent {
    /// An event from a cell-based front end (the TUI): the position is the cell.
    pub const fn at_cell(kind: MouseEventKind, column: u16, row: u16, modifiers: Modifiers) -> Self {
        Self { kind, column, row, x: column as f32, y: row as f32, modifiers }
    }

    /// An event at a precise position in cell units (a pixel front end); the
    /// cell is the one containing the point, clamped to the `u16` range.
    pub fn at(kind: MouseEventKind, x: f32, y: f32, modifiers: Modifiers) -> Self {
        // Float-to-int `as` saturates (and maps NaN to 0).
        Self { kind, column: x.floor() as u16, row: y.floor() as u16, x, y, modifiers }
    }
}

/// The character a key contributes to a paste, if it can be part of one.
pub fn paste_char(key: &KeyEvent) -> Option<char> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let plain = key.modifiers.difference(Modifiers::SHIFT).is_empty();
    match key.code {
        KeyCode::Char(c) if plain => Some(c),
        KeyCode::Enter if plain => Some('\n'),
        KeyCode::Tab if plain => Some('\t'),
        _ => None,
    }
}

/// A burst containing a newline and several printable keys that arrived in one
/// read is a paste, not typing: delivering it as keys would submit early. (The
/// Windows console delivers a paste as individual key presses.)
pub fn coalesce(batch: Vec<InputEvent>) -> Vec<InputEvent> {
    let presses: Vec<Option<char>> = batch
        .iter()
        .filter_map(|e| match e {
            InputEvent::Key(k) if k.kind != KeyEventKind::Release => Some(paste_char(k)),
            _ => None,
        })
        .collect();
    let all_text = !presses.is_empty() && presses.iter().all(Option::is_some);
    // A newline only at the very end is a typed/pasted line followed by Enter,
    // which must reach the application as a real Enter key.
    let inner_newline = presses.len() > 1 && presses[..presses.len() - 1].contains(&Some('\n'));
    if all_text && inner_newline && presses.len() >= 3 {
        let text: String = presses.into_iter().flatten().collect();
        let mut out: Vec<InputEvent> = batch.into_iter().filter(|e| !matches!(e, InputEvent::Key(_))).collect();
        out.push(InputEvent::Paste(text.trim_end_matches('\n').to_owned()));
        return out;
    }
    batch
}

/// Modifier bits for the terminal protocol (Super has no wire bit).
pub fn mods(modifiers: Modifiers) -> u8 {
    let mut m = 0;
    if modifiers.contains(Modifiers::SHIFT) {
        m |= MOD_SHIFT;
    }
    if modifiers.contains(Modifiers::ALT) {
        m |= MOD_ALT;
    }
    if modifiers.contains(Modifiers::CONTROL) {
        m |= MOD_CTRL;
    }
    m
}

/// Converts a key event into the wire representation for a terminal.
pub fn to_wire(key: &KeyEvent) -> Option<(Key, u8)> {
    let mut m = mods(key.modifiers);
    let wire = match key.code {
        KeyCode::Char(c) => {
            // AltGr arrives as Ctrl+Alt with an already-composed character.
            if m & MOD_CTRL != 0 && m & MOD_ALT != 0 && !c.is_ascii_alphabetic() {
                m &= !(MOD_CTRL | MOD_ALT);
            }
            // The character already reflects Shift.
            m &= !MOD_SHIFT;
            Key::Char(c)
        }
        KeyCode::Enter => Key::Enter,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Esc => Key::Esc,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Insert => Key::Insert,
        KeyCode::Delete => Key::Delete,
        KeyCode::F(n) => Key::F(n),
        KeyCode::Null => return None,
    };
    Some((wire, m))
}

/// Ctrl+] (also reported as Ctrl+5 or a raw GS character by some consoles).
pub fn is_leave_terminal(key: &KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(Modifiers::CONTROL);
    matches!(key.code, KeyCode::Char('\x1d')) || (ctrl && matches!(key.code, KeyCode::Char(']') | KeyCode::Char('5')))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: Modifiers) -> InputEvent {
        InputEvent::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn bursts_with_newlines_become_pastes() {
        let batch = vec![key(KeyCode::Char('a'), Modifiers::NONE), key(KeyCode::Enter, Modifiers::NONE), key(KeyCode::Char('B'), Modifiers::SHIFT)];
        match coalesce(batch).as_slice() {
            [InputEvent::Paste(text)] => assert_eq!(text, "a\nB"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn pastes_keep_other_events_and_drop_releases() {
        let release = InputEvent::Key(KeyEvent::with_kind(KeyCode::Char('a'), Modifiers::NONE, KeyEventKind::Release));
        let batch = vec![
            key(KeyCode::Char('a'), Modifiers::NONE),
            release,
            InputEvent::Resize(80, 24),
            key(KeyCode::Tab, Modifiers::NONE),
            key(KeyCode::Enter, Modifiers::NONE),
            key(KeyCode::Char('b'), Modifiers::NONE),
            key(KeyCode::Enter, Modifiers::NONE),
        ];
        assert_eq!(coalesce(batch), vec![InputEvent::Resize(80, 24), InputEvent::Paste("a\t\nb".into())]);
    }

    #[test]
    fn typing_and_shortcuts_are_not_pastes() {
        let typed = coalesce(vec![key(KeyCode::Char('a'), Modifiers::NONE), key(KeyCode::Char('b'), Modifiers::NONE)]);
        assert_eq!(typed.len(), 2);
        let line_then_enter =
            coalesce(vec![key(KeyCode::Char('a'), Modifiers::NONE), key(KeyCode::Char('b'), Modifiers::NONE), key(KeyCode::Enter, Modifiers::NONE)]);
        assert_eq!(line_then_enter.len(), 3, "a trailing Enter stays a key press");
        let enter = coalesce(vec![key(KeyCode::Enter, Modifiers::NONE)]);
        assert!(matches!(enter.as_slice(), [InputEvent::Key(_)]));
        let shortcut =
            coalesce(vec![key(KeyCode::Char('a'), Modifiers::NONE), key(KeyCode::Enter, Modifiers::NONE), key(KeyCode::Char('c'), Modifiers::CONTROL)]);
        assert_eq!(shortcut.len(), 3, "control keys break paste detection");
    }

    #[test]
    fn paste_chars_allow_only_shift() {
        assert_eq!(paste_char(&KeyEvent::new(KeyCode::Char('X'), Modifiers::SHIFT)), Some('X'));
        assert_eq!(paste_char(&KeyEvent::new(KeyCode::Enter, Modifiers::NONE)), Some('\n'));
        assert_eq!(paste_char(&KeyEvent::new(KeyCode::Tab, Modifiers::NONE)), Some('\t'));
        assert_eq!(paste_char(&KeyEvent::new(KeyCode::Char('x'), Modifiers::ALT)), None);
        assert_eq!(paste_char(&KeyEvent::new(KeyCode::Char('x'), Modifiers::SUPER)), None);
        assert_eq!(paste_char(&KeyEvent::new(KeyCode::Up, Modifiers::NONE)), None);
        assert_eq!(paste_char(&KeyEvent::with_kind(KeyCode::Char('x'), Modifiers::NONE, KeyEventKind::Release)), None);
    }

    #[test]
    fn wire_conversion_handles_altgr_and_shift() {
        let altgr = KeyEvent::new(KeyCode::Char('@'), Modifiers::CONTROL | Modifiers::ALT);
        assert_eq!(to_wire(&altgr), Some((Key::Char('@'), 0)));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), Modifiers::CONTROL);
        assert_eq!(to_wire(&ctrl_c), Some((Key::Char('c'), MOD_CTRL)));
        let upper = KeyEvent::new(KeyCode::Char('A'), Modifiers::SHIFT);
        assert_eq!(to_wire(&upper), Some((Key::Char('A'), 0)));
        let shift_up = KeyEvent::new(KeyCode::Up, Modifiers::SHIFT);
        assert_eq!(to_wire(&shift_up), Some((Key::Up, MOD_SHIFT)));
        let ctrl_alt_a = KeyEvent::new(KeyCode::Char('a'), Modifiers::CONTROL | Modifiers::ALT);
        assert_eq!(to_wire(&ctrl_alt_a), Some((Key::Char('a'), MOD_CTRL | MOD_ALT)), "Ctrl+Alt+letter is a shortcut, not AltGr");
        assert_eq!(to_wire(&KeyEvent::new(KeyCode::F(5), Modifiers::SUPER)), Some((Key::F(5), 0)));
        assert_eq!(to_wire(&KeyEvent::new(KeyCode::Null, Modifiers::NONE)), None);
        assert!(is_leave_terminal(&KeyEvent::new(KeyCode::Char(']'), Modifiers::CONTROL)));
        assert!(is_leave_terminal(&KeyEvent::new(KeyCode::Char('5'), Modifiers::CONTROL)));
        assert!(is_leave_terminal(&KeyEvent::new(KeyCode::Char('\x1d'), Modifiers::NONE)));
        assert!(!is_leave_terminal(&KeyEvent::new(KeyCode::Char(']'), Modifiers::NONE)));
    }

    #[test]
    fn modifier_set_operations() {
        let both = Modifiers::CONTROL | Modifiers::SHIFT;
        assert!(both.contains(Modifiers::CONTROL) && both.contains(Modifiers::SHIFT));
        assert!(!both.contains(Modifiers::CONTROL | Modifiers::ALT));
        assert!(both.intersects(Modifiers::CONTROL | Modifiers::ALT));
        assert!(!both.intersects(Modifiers::ALT | Modifiers::SUPER));
        assert_eq!(both - Modifiers::SHIFT, Modifiers::CONTROL);
        assert_eq!(both & Modifiers::SHIFT, Modifiers::SHIFT);
        assert!(Modifiers::NONE.is_empty() && Modifiers::default().is_empty());
        assert!(Modifiers::NONE.contains(Modifiers::NONE) && !Modifiers::NONE.intersects(Modifiers::NONE));
        let mut m = both;
        m.remove(Modifiers::CONTROL);
        assert_eq!(m, Modifiers::SHIFT);
        m |= Modifiers::SUPER;
        assert_eq!(m.bits(), Modifiers::SHIFT.bits() | Modifiers::SUPER.bits());
        assert_eq!(!Modifiers::NONE, Modifiers::SHIFT | Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER);
        assert_eq!(Modifiers::from_bits_truncate(0xff), !Modifiers::NONE);
        assert_eq!(format!("{:?}", Modifiers::CONTROL | Modifiers::ALT), "Modifiers(CONTROL | ALT)");
        assert_eq!(format!("{:?}", Modifiers::NONE), "Modifiers(NONE)");
        assert_eq!(mods(Modifiers::SHIFT | Modifiers::ALT | Modifiers::CONTROL | Modifiers::SUPER), MOD_SHIFT | MOD_ALT | MOD_CTRL);
    }

    #[test]
    fn mouse_positions() {
        let cell = MouseEvent::at_cell(MouseEventKind::Down(MouseButton::Left), 7, 3, Modifiers::NONE);
        assert_eq!((cell.column, cell.row, cell.x, cell.y), (7, 3, 7.0, 3.0));
        let precise = MouseEvent::at(MouseEventKind::Moved, 7.75, 3.25, Modifiers::SHIFT);
        assert_eq!((precise.column, precise.row), (7, 3));
        assert_eq!((precise.x, precise.y), (7.75, 3.25));
        let outside = MouseEvent::at(MouseEventKind::Moved, -0.5, 1e9, Modifiers::NONE);
        assert_eq!((outside.column, outside.row), (0, u16::MAX));
    }
}
