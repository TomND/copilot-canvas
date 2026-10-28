//! Input thread: reads console events, batches them and detects pastes that the
//! Windows console delivers as individual key presses.

use std::time::Duration;

use crossbeam_channel::Sender;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::protocol::{Key, MOD_ALT, MOD_CTRL, MOD_SHIFT};

#[derive(Debug, Clone)]
pub enum InputEvent {
    Event(Event),
    Paste(String),
}

fn paste_char(key: &KeyEvent) -> Option<char> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
    match key.code {
        KeyCode::Char(c) if plain => Some(c),
        KeyCode::Enter if plain => Some('\n'),
        KeyCode::Tab if plain => Some('\t'),
        _ => None,
    }
}

/// A burst containing a newline and several printable keys that arrived in one
/// read is a paste, not typing: delivering it as keys would submit early.
pub fn coalesce(batch: Vec<Event>) -> Vec<InputEvent> {
    let presses: Vec<Option<char>> = batch
        .iter()
        .filter_map(|e| match e {
            Event::Key(k) if k.kind != KeyEventKind::Release => Some(paste_char(k)),
            _ => None,
        })
        .collect();
    let all_text = !presses.is_empty() && presses.iter().all(Option::is_some);
    // A newline only at the very end is a typed/pasted line followed by Enter,
    // which must reach the application as a real Enter key.
    let inner_newline = presses.len() > 1 && presses[..presses.len() - 1].contains(&Some('\n'));
    if all_text && inner_newline && presses.len() >= 3 {
        let text: String = presses.into_iter().flatten().collect();
        let mut out: Vec<InputEvent> =
            batch.into_iter().filter(|e| !matches!(e, Event::Key(_))).map(InputEvent::Event).collect();
        out.push(InputEvent::Paste(text.trim_end_matches('\n').to_owned()));
        return out;
    }
    batch.into_iter().map(InputEvent::Event).collect()
}

pub fn spawn(tx: Sender<Vec<InputEvent>>) {
    std::thread::Builder::new()
        .name("input".into())
        .spawn(move || {
            while let Ok(first) = event::read() {
                let mut batch = vec![first];
                while batch.len() < 4096 && event::poll(Duration::ZERO).unwrap_or(false) {
                    match event::read() {
                        Ok(event) => batch.push(event),
                        Err(_) => break,
                    }
                }
                if tx.send(coalesce(batch)).is_err() {
                    break;
                }
            }
        })
        .expect("spawn input thread");
}

pub fn mods(modifiers: KeyModifiers) -> u8 {
    let mut m = 0;
    if modifiers.contains(KeyModifiers::SHIFT) {
        m |= MOD_SHIFT;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        m |= MOD_ALT;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        m |= MOD_CTRL;
    }
    m
}

/// Converts a console key event into the wire representation for a terminal.
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
        _ => return None,
    };
    Some((wire, m))
}

/// Ctrl+] (also reported as Ctrl+5 or a raw GS character by some consoles).
pub fn is_leave_terminal(key: &KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    matches!(key.code, KeyCode::Char('\x1d')) || (ctrl && matches!(key.code, KeyCode::Char(']') | KeyCode::Char('5')))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn bursts_with_newlines_become_pastes() {
        let batch = vec![
            key(KeyCode::Char('a'), KeyModifiers::NONE),
            key(KeyCode::Enter, KeyModifiers::NONE),
            key(KeyCode::Char('B'), KeyModifiers::SHIFT),
        ];
        match coalesce(batch).as_slice() {
            [InputEvent::Paste(text)] => assert_eq!(text, "a\nB"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn typing_and_shortcuts_are_not_pastes() {
        let typed = coalesce(vec![key(KeyCode::Char('a'), KeyModifiers::NONE), key(KeyCode::Char('b'), KeyModifiers::NONE)]);
        assert_eq!(typed.len(), 2);
        let line_then_enter = coalesce(vec![
            key(KeyCode::Char('a'), KeyModifiers::NONE),
            key(KeyCode::Char('b'), KeyModifiers::NONE),
            key(KeyCode::Enter, KeyModifiers::NONE),
        ]);
        assert_eq!(line_then_enter.len(), 3, "a trailing Enter stays a key press");
        let enter = coalesce(vec![key(KeyCode::Enter, KeyModifiers::NONE)]);
        assert!(matches!(enter.as_slice(), [InputEvent::Event(_)]));
        let shortcut = coalesce(vec![
            key(KeyCode::Char('a'), KeyModifiers::NONE),
            key(KeyCode::Enter, KeyModifiers::NONE),
            key(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ]);
        assert_eq!(shortcut.len(), 3, "control keys break paste detection");
    }

    #[test]
    fn wire_conversion_handles_altgr_and_shift() {
        let altgr = KeyEvent::new(KeyCode::Char('@'), KeyModifiers::CONTROL | KeyModifiers::ALT);
        assert_eq!(to_wire(&altgr), Some((Key::Char('@'), 0)));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(to_wire(&ctrl_c), Some((Key::Char('c'), MOD_CTRL)));
        let upper = KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT);
        assert_eq!(to_wire(&upper), Some((Key::Char('A'), 0)));
        let shift_up = KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT);
        assert_eq!(to_wire(&shift_up), Some((Key::Up, MOD_SHIFT)));
        assert!(is_leave_terminal(&KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL)));
        assert!(is_leave_terminal(&KeyEvent::new(KeyCode::Char('5'), KeyModifiers::CONTROL)));
        assert!(!is_leave_terminal(&KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE)));
    }
}
