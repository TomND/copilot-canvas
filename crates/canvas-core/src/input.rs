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
    /// Text typed at once (an IME composition committed): a terminal gets
    /// the characters, a text field inserts them.
    Text(String),
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
    /// Whose key it is ([`KeyPolicy`]): the canvas's or the program's in a
    /// terminal, or decided by today's rules.
    pub source: KeySource,
}

impl KeyEvent {
    /// A key press.
    pub const fn new(code: KeyCode, modifiers: Modifiers) -> Self {
        Self { code, modifiers, kind: KeyEventKind::Press, source: KeySource::Any }
    }

    pub const fn with_kind(code: KeyCode, modifiers: Modifiers, kind: KeyEventKind) -> Self {
        Self { code, modifiers, kind, source: KeySource::Any }
    }

    pub const fn from(self, source: KeySource) -> Self {
        Self { source, ..self }
    }
}

/// Who a key belongs to when a terminal has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum KeySource {
    /// Windows' rules (and the TUI's): Ctrl+C copies a selection, Ctrl+V
    /// pastes, every other key reaches the program.
    #[default]
    Any,
    /// An app shortcut (macOS: the Command key): never the program's.
    App,
    /// The program's (macOS: the Control key): reaches it untouched, but for
    /// Ctrl+] and Ctrl+F6.
    Program,
}

/// Which modifier is the app's (MAC-LINUX-PLAN D8): Ctrl on Windows and
/// Linux; Command on macOS, where every Ctrl combination goes to the program,
/// Command+digit picks a view (Option+digit types characters) and a
/// Command-click is the Ctrl-click (a Ctrl-click is the secondary click).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyPolicy {
    #[default]
    CtrlApp,
    CommandApp,
}

impl KeyPolicy {
    /// A key as the core's Ctrl-based shortcuts expect it.
    pub fn key(self, key: KeyEvent) -> KeyEvent {
        if self == KeyPolicy::CtrlApp {
            return key;
        }
        let mods = key.modifiers;
        if mods.contains(Modifiers::SUPER) {
            let rest = mods.difference(Modifiers::SUPER);
            let modifiers = match key.code {
                // Command+1…9: the activity bar's views (Alt+1…9 elsewhere).
                KeyCode::Char('1'..='9') if rest.is_empty() => Modifiers::ALT,
                // Command+N: a new window (Ctrl+Shift+N elsewhere).
                KeyCode::Char('n' | 'N') => Modifiers::CONTROL | Modifiers::SHIFT,
                _ => rest.union(Modifiers::CONTROL),
            };
            let code = match key.code {
                KeyCode::Char(c) if modifiers.contains(Modifiers::CONTROL) => KeyCode::Char(c.to_ascii_lowercase()),
                other => other,
            };
            return KeyEvent { code, modifiers, ..key }.from(KeySource::App);
        }
        if mods.contains(Modifiers::CONTROL) {
            return key.from(KeySource::Program);
        }
        key
    }

    /// A mouse event as the core expects it: Command acts as Ctrl
    /// (Command-click opens links and toggles selection); a Ctrl-click is
    /// the secondary (right) click.
    pub fn mouse(self, mut event: MouseEvent) -> MouseEvent {
        if self == KeyPolicy::CtrlApp {
            return event;
        }
        let mods = event.modifiers;
        let left = matches!(event.kind, MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left));
        if left && mods.contains(Modifiers::CONTROL) && !mods.contains(Modifiers::SUPER) {
            event.kind = match event.kind {
                MouseEventKind::Down(_) => MouseEventKind::Down(MouseButton::Right),
                MouseEventKind::Up(_) => MouseEventKind::Up(MouseButton::Right),
                _ => MouseEventKind::Drag(MouseButton::Right),
            };
            event.modifiers = mods.difference(Modifiers::CONTROL);
        } else if mods.contains(Modifiers::SUPER) {
            event.modifiers = mods.difference(Modifiers::SUPER).union(Modifiers::CONTROL);
        }
        event
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

/// The character a key contributes to a paste, if it can be part of one:
/// text (Shift allowed; AltGr compositions arrive with Ctrl+Alt), Enter, Tab.
pub fn paste_char(key: &KeyEvent) -> Option<char> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let plain = key.modifiers.difference(Modifiers::SHIFT).is_empty();
    let altgr = key.modifiers.difference(Modifiers::SHIFT) == Modifiers::CONTROL | Modifiers::ALT;
    match key.code {
        KeyCode::Char(c) if plain && !c.is_control() => Some(c),
        KeyCode::Char(c) if altgr && !c.is_control() && !c.is_ascii_alphabetic() => Some(c),
        KeyCode::Enter if plain => Some('\n'),
        KeyCode::Tab if plain => Some('\t'),
        _ => None,
    }
}

/// Pasted text without its trailing line breaks: a paste never ends with
/// Enter, so pasting a copied line does not send it.
pub fn paste_body(text: &str) -> &str {
    text.trim_end_matches(['\r', '\n'])
}

/// How long the console reader waits for the rest of a burst of keys, and
/// how soon after a burst a batch still continues it. Pasted text arrives in
/// a few writes microseconds apart; nobody types two keys this fast.
pub const BURST_GAP: std::time::Duration = std::time::Duration::from_millis(8);

/// The key events of a batch, in order.
fn keys(batch: &[InputEvent]) -> impl Iterator<Item = &KeyEvent> {
    batch.iter().filter_map(|e| match e {
        InputEvent::Key(k) => Some(k),
        _ => None,
    })
}

/// The batch's key presses are all text ([`paste_char`]), at least one.
pub fn is_text(batch: &[InputEvent]) -> bool {
    let mut presses = keys(batch).filter(|k| k.kind != KeyEventKind::Release).peekable();
    presses.peek().is_some() && presses.all(|k| paste_char(k).is_some())
}

/// The console reader should keep reading for [`BURST_GAP`] before it
/// decides: the batch is text with at least two key presses, or it
/// `continues` a text batch read less than [`BURST_GAP`] before.
pub fn is_burst(batch: &[InputEvent], continues: bool) -> bool {
    is_text(batch) && (continues || keys(batch).filter(|k| k.kind != KeyEventKind::Release).count() >= 2)
}

/// Each key is released before the next is pressed: the Windows console
/// delivers pasted text as press/release pairs, while keys typed in a row
/// overlap, and the last typed key is usually still down when it is read.
fn paired(batch: &[InputEvent]) -> bool {
    let same = |a: KeyCode, b: KeyCode| match (a, b) {
        (KeyCode::Char(x), KeyCode::Char(y)) => x.to_lowercase().eq(y.to_lowercase()),
        _ => a == b,
    };
    let mut held: Option<KeyCode> = None;
    for key in keys(batch) {
        match key.kind {
            KeyEventKind::Release => {
                if held.is_some_and(|h| same(h, key.code)) {
                    held = None;
                }
            }
            _ if held.is_some() => return false,
            _ => held = Some(key.code),
        }
    }
    held.is_none()
}

/// Turns a burst of keys from the Windows console (which delivers a paste
/// as individual key presses) into one [`InputEvent::Paste`], so that no
/// pasted line break reaches the program as Enter. The rule:
///
/// - the batch is a burst ([`is_burst`]: text keys only, at least two
///   presses, or continuing a text batch that came less than [`BURST_GAP`]
///   earlier, i.e. a paste the console split across reads);
/// - it contains a line break (Enter), anywhere, also at the end;
/// - its keys come in press/release pairs ([`paired`]).
///
/// Typing never meets it: keys typed one after another arrive in batches of
/// their own (one press each), and keys that piled up while the reader was
/// busy overlap or end with a key still held. Bursts without a line break
/// stay keys (harmless). The paste keeps its text as typed (`\n` for each
/// Enter); [`paste_body`] drops the trailing line breaks when it is sent.
pub fn coalesce(batch: Vec<InputEvent>, continues: bool) -> Vec<InputEvent> {
    let text: String = keys(&batch).filter_map(paste_char).collect();
    if !is_burst(&batch, continues) || !text.contains('\n') || !paired(&batch) {
        return batch;
    }
    let mut out: Vec<InputEvent> = batch.into_iter().filter(|e| !matches!(e, InputEvent::Key(_))).collect();
    out.push(InputEvent::Paste(text));
    out
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

    #[test]
    fn the_command_key_is_the_apps_on_macos_and_ctrl_the_programs() {
        let mac = KeyPolicy::CommandApp;
        let key = |c: char, m: Modifiers| mac.key(KeyEvent::new(KeyCode::Char(c), m));
        assert_eq!(key('c', Modifiers::SUPER), KeyEvent::new(KeyCode::Char('c'), Modifiers::CONTROL).from(KeySource::App));
        assert_eq!(key('Z', Modifiers::SUPER | Modifiers::SHIFT), KeyEvent::new(KeyCode::Char('z'), Modifiers::CONTROL | Modifiers::SHIFT).from(KeySource::App));
        assert_eq!(key('3', Modifiers::SUPER), KeyEvent::new(KeyCode::Char('3'), Modifiers::ALT).from(KeySource::App));
        assert_eq!(key('n', Modifiers::SUPER), KeyEvent::new(KeyCode::Char('n'), Modifiers::CONTROL | Modifiers::SHIFT).from(KeySource::App));
        assert_eq!(key('c', Modifiers::CONTROL).source, KeySource::Program);
        assert_eq!(key('x', Modifiers::ALT).source, KeySource::Any, "Option (as Alt) is unchanged");
        assert_eq!(KeyPolicy::CtrlApp.key(KeyEvent::new(KeyCode::Char('c'), Modifiers::CONTROL)).source, KeySource::Any);
        let click = |m: Modifiers| mac.mouse(MouseEvent::at(MouseEventKind::Down(MouseButton::Left), 1.0, 1.0, m));
        assert_eq!(click(Modifiers::CONTROL).kind, MouseEventKind::Down(MouseButton::Right));
        assert_eq!(click(Modifiers::SUPER).modifiers, Modifiers::CONTROL);
    }

    fn key(code: KeyCode, modifiers: Modifiers) -> InputEvent {
        InputEvent::Key(KeyEvent::new(code, modifiers))
    }

    fn release(code: KeyCode) -> InputEvent {
        InputEvent::Key(KeyEvent::with_kind(code, Modifiers::NONE, KeyEventKind::Release))
    }

    fn code(c: char) -> KeyCode {
        match c {
            '\n' => KeyCode::Enter,
            '\t' => KeyCode::Tab,
            c => KeyCode::Char(c),
        }
    }

    /// Text as the Windows console delivers a paste: a press and a release per character.
    fn pasted(text: &str) -> Vec<InputEvent> {
        text.chars()
            .flat_map(|c| {
                let shift = if c.is_ascii_uppercase() { Modifiers::SHIFT } else { Modifiers::NONE };
                [key(code(c), shift), release(code(c))]
            })
            .collect()
    }

    #[test]
    fn pasted_bursts_become_one_paste_with_their_trailing_line_break() {
        assert_eq!(coalesce(pasted("a\nB\n"), false), vec![InputEvent::Paste("a\nB\n".into())]);
        assert_eq!(coalesce(pasted("x\n"), false), vec![InputEvent::Paste("x\n".into())], "short pastes too");
        assert_eq!(coalesce(pasted("\n\n  indented\n"), false), vec![InputEvent::Paste("\n\n  indented\n".into())]);
        assert_eq!(paste_body("a\nB\n"), "a\nB");
        assert_eq!(paste_body("x\r\n\r\n"), "x");
        assert_eq!(paste_body("\r\n"), "");
        assert_eq!(paste_body("a\n\nb"), "a\n\nb", "inner blank lines stay");
    }

    #[test]
    fn a_paste_split_across_reads_continues_as_a_paste() {
        // The console wrote "a", then "\n", then "b": the first batch was a single key.
        assert_eq!(coalesce(pasted("a"), false), pasted("a"));
        assert!(is_burst(&pasted("\n"), true), "a text batch right after another is read as a burst");
        assert_eq!(coalesce(pasted("\n"), true), vec![InputEvent::Paste("\n".into())], "its line break is not Enter");
        assert_eq!(coalesce(pasted("\nb"), true), vec![InputEvent::Paste("\nb".into())]);
        assert!(!is_burst(&pasted("\n"), false), "on its own it is a key");
    }

    #[test]
    fn typing_and_shortcuts_are_not_pastes() {
        assert_eq!(coalesce(pasted("\n"), false), pasted("\n"), "Enter on its own stays Enter");
        assert_eq!(coalesce(vec![key(KeyCode::Enter, Modifiers::NONE)], false).len(), 1);
        assert_eq!(coalesce(pasted("abc"), false), pasted("abc"), "a burst without a line break stays keys");
        // Typed keys that piled up while the reader was busy: the Enter is still held.
        let held = vec![key(code('l'), Modifiers::NONE), release(code('l')), key(code('s'), Modifiers::NONE), release(code('s')), key(KeyCode::Enter, Modifiers::NONE)];
        assert_eq!(coalesce(held.clone(), false), held);
        // Overlapping keys (rolled over while typing).
        let rolled = vec![key(code('l'), Modifiers::NONE), key(code('s'), Modifiers::NONE), release(code('l')), release(code('s')), key(KeyCode::Enter, Modifiers::NONE), release(KeyCode::Enter)];
        assert_eq!(coalesce(rolled.clone(), false), rolled);
        // Presses without releases (a console that reports none) overlap too.
        let unreleased = vec![key(code('a'), Modifiers::NONE), key(KeyCode::Enter, Modifiers::NONE), key(code('b'), Modifiers::NONE)];
        assert_eq!(coalesce(unreleased.clone(), false), unreleased);
        let mut shortcut = pasted("a\n");
        shortcut.push(key(KeyCode::Char('c'), Modifiers::CONTROL));
        assert_eq!(coalesce(shortcut.clone(), false), shortcut, "control keys break paste detection");
        assert!(!is_burst(&[key(KeyCode::Up, Modifiers::NONE)], true));
    }

    #[test]
    fn pastes_keep_other_events() {
        let mut batch = vec![InputEvent::Resize(80, 24)];
        batch.extend(pasted("a\tb\n"));
        assert_eq!(coalesce(batch, false), vec![InputEvent::Resize(80, 24), InputEvent::Paste("a\tb\n".into())]);
    }

    #[test]
    fn paste_chars_allow_only_shift_and_altgr() {
        assert_eq!(paste_char(&KeyEvent::new(KeyCode::Char('X'), Modifiers::SHIFT)), Some('X'));
        assert_eq!(paste_char(&KeyEvent::new(KeyCode::Enter, Modifiers::NONE)), Some('\n'));
        assert_eq!(paste_char(&KeyEvent::new(KeyCode::Tab, Modifiers::NONE)), Some('\t'));
        assert_eq!(paste_char(&KeyEvent::new(KeyCode::Char('{'), Modifiers::CONTROL | Modifiers::ALT)), Some('{'), "AltGr");
        assert_eq!(paste_char(&KeyEvent::new(KeyCode::Char('x'), Modifiers::CONTROL | Modifiers::ALT)), None);
        assert_eq!(paste_char(&KeyEvent::new(KeyCode::Char('x'), Modifiers::ALT)), None);
        assert_eq!(paste_char(&KeyEvent::new(KeyCode::Char('x'), Modifiers::SUPER)), None);
        assert_eq!(paste_char(&KeyEvent::new(KeyCode::Char('\x1d'), Modifiers::NONE)), None);
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
