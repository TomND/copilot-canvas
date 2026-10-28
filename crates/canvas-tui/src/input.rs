//! Console input for the TUI: reads crossterm events on a thread, converts them
//! to the core's platform-neutral [`InputEvent`]s and batches them (detecting
//! pastes the Windows console delivers as individual key presses).

use std::time::Duration;

use crossbeam_channel::Sender;
use ratatui::crossterm::event::{self as ct, Event};

use canvas_core::input::{self, InputEvent, KeyCode, KeyEvent, KeyEventKind, Modifiers, MouseButton, MouseEvent, MouseEventKind};

pub fn spawn(tx: Sender<Vec<InputEvent>>) {
    std::thread::Builder::new()
        .name("input".into())
        .spawn(move || {
            while let Ok(first) = ct::read() {
                let mut batch = vec![first];
                while batch.len() < 4096 && ct::poll(Duration::ZERO).unwrap_or(false) {
                    match ct::read() {
                        Ok(event) => batch.push(event),
                        Err(_) => break,
                    }
                }
                let events: Vec<InputEvent> = batch.into_iter().filter_map(from_crossterm).collect();
                if events.is_empty() {
                    continue;
                }
                if tx.send(input::coalesce(events)).is_err() {
                    break;
                }
            }
        })
        .expect("spawn input thread");
}

/// Converts a console event; `None` for keys the core has no use for (media,
/// lone modifier and lock keys, which the Windows console never reports anyway).
pub fn from_crossterm(event: Event) -> Option<InputEvent> {
    Some(match event {
        Event::Key(key) => InputEvent::Key(key_event(key)?),
        Event::Mouse(mouse) => InputEvent::Mouse(mouse_event(mouse)),
        Event::Paste(text) => InputEvent::Paste(text),
        Event::FocusGained => InputEvent::Focus(true),
        Event::FocusLost => InputEvent::Focus(false),
        Event::Resize(columns, rows) => InputEvent::Resize(columns, rows),
    })
}

fn modifiers(m: ct::KeyModifiers) -> Modifiers {
    let mut out = Modifiers::NONE;
    for (from, to) in [
        (ct::KeyModifiers::SHIFT, Modifiers::SHIFT),
        (ct::KeyModifiers::CONTROL, Modifiers::CONTROL),
        (ct::KeyModifiers::ALT, Modifiers::ALT),
        (ct::KeyModifiers::SUPER, Modifiers::SUPER),
    ] {
        if m.contains(from) {
            out |= to;
        }
    }
    out
}

fn key_event(key: ct::KeyEvent) -> Option<KeyEvent> {
    let code = match key.code {
        // AltGr characters arrive as Char with CONTROL | ALT; they pass through
        // unchanged and `input::to_wire` recognises them.
        ct::KeyCode::Char(c) => KeyCode::Char(c),
        ct::KeyCode::Enter => KeyCode::Enter,
        ct::KeyCode::Tab => KeyCode::Tab,
        ct::KeyCode::BackTab => KeyCode::BackTab,
        ct::KeyCode::Backspace => KeyCode::Backspace,
        ct::KeyCode::Esc => KeyCode::Esc,
        ct::KeyCode::Left => KeyCode::Left,
        ct::KeyCode::Right => KeyCode::Right,
        ct::KeyCode::Up => KeyCode::Up,
        ct::KeyCode::Down => KeyCode::Down,
        ct::KeyCode::Home => KeyCode::Home,
        ct::KeyCode::End => KeyCode::End,
        ct::KeyCode::PageUp => KeyCode::PageUp,
        ct::KeyCode::PageDown => KeyCode::PageDown,
        ct::KeyCode::Insert => KeyCode::Insert,
        ct::KeyCode::Delete => KeyCode::Delete,
        ct::KeyCode::F(n) => KeyCode::F(n),
        ct::KeyCode::Null => KeyCode::Null,
        _ => return None,
    };
    let kind = match key.kind {
        ct::KeyEventKind::Press => KeyEventKind::Press,
        ct::KeyEventKind::Repeat => KeyEventKind::Repeat,
        ct::KeyEventKind::Release => KeyEventKind::Release,
    };
    Some(KeyEvent::with_kind(code, modifiers(key.modifiers), kind))
}

fn button(button: ct::MouseButton) -> MouseButton {
    match button {
        ct::MouseButton::Left => MouseButton::Left,
        ct::MouseButton::Right => MouseButton::Right,
        ct::MouseButton::Middle => MouseButton::Middle,
    }
}

fn mouse_event(mouse: ct::MouseEvent) -> MouseEvent {
    let kind = match mouse.kind {
        ct::MouseEventKind::Down(b) => MouseEventKind::Down(button(b)),
        ct::MouseEventKind::Up(b) => MouseEventKind::Up(button(b)),
        ct::MouseEventKind::Drag(b) => MouseEventKind::Drag(button(b)),
        ct::MouseEventKind::Moved => MouseEventKind::Moved,
        ct::MouseEventKind::ScrollUp => MouseEventKind::ScrollUp,
        ct::MouseEventKind::ScrollDown => MouseEventKind::ScrollDown,
        ct::MouseEventKind::ScrollLeft => MouseEventKind::ScrollLeft,
        ct::MouseEventKind::ScrollRight => MouseEventKind::ScrollRight,
    };
    MouseEvent::at_cell(kind, mouse.column, mouse.row, modifiers(mouse.modifiers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use canvas_core::protocol::{Key, MOD_ALT, MOD_CTRL};

    fn ct_key(code: ct::KeyCode, modifiers: ct::KeyModifiers) -> Event {
        Event::Key(ct::KeyEvent::new(code, modifiers))
    }

    fn key(event: Event) -> KeyEvent {
        match from_crossterm(event) {
            Some(InputEvent::Key(key)) => key,
            other => panic!("expected a key, got {other:?}"),
        }
    }

    fn mouse(kind: ct::MouseEventKind, column: u16, row: u16, modifiers: ct::KeyModifiers) -> MouseEvent {
        match from_crossterm(Event::Mouse(ct::MouseEvent { kind, column, row, modifiers })) {
            Some(InputEvent::Mouse(mouse)) => mouse,
            other => panic!("expected a mouse event, got {other:?}"),
        }
    }

    #[test]
    fn keys_and_modifiers_convert() {
        let ctrl_c = key(ct_key(ct::KeyCode::Char('c'), ct::KeyModifiers::CONTROL));
        assert_eq!(ctrl_c, KeyEvent::new(KeyCode::Char('c'), Modifiers::CONTROL));
        let upper = key(ct_key(ct::KeyCode::Char('A'), ct::KeyModifiers::SHIFT));
        assert_eq!(upper, KeyEvent::new(KeyCode::Char('A'), Modifiers::SHIFT));
        let all = ct::KeyModifiers::SHIFT | ct::KeyModifiers::CONTROL | ct::KeyModifiers::ALT | ct::KeyModifiers::SUPER;
        let f5 = key(ct_key(ct::KeyCode::F(5), all));
        assert_eq!(f5, KeyEvent::new(KeyCode::F(5), Modifiers::SHIFT | Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER));
        let hyper = key(ct_key(ct::KeyCode::Esc, ct::KeyModifiers::HYPER | ct::KeyModifiers::META));
        assert_eq!(hyper.modifiers, Modifiers::NONE, "modifiers the core does not model are dropped");
        let pairs = [
            (ct::KeyCode::Enter, KeyCode::Enter),
            (ct::KeyCode::Tab, KeyCode::Tab),
            (ct::KeyCode::BackTab, KeyCode::BackTab),
            (ct::KeyCode::Backspace, KeyCode::Backspace),
            (ct::KeyCode::Esc, KeyCode::Esc),
            (ct::KeyCode::Left, KeyCode::Left),
            (ct::KeyCode::Right, KeyCode::Right),
            (ct::KeyCode::Up, KeyCode::Up),
            (ct::KeyCode::Down, KeyCode::Down),
            (ct::KeyCode::Home, KeyCode::Home),
            (ct::KeyCode::End, KeyCode::End),
            (ct::KeyCode::PageUp, KeyCode::PageUp),
            (ct::KeyCode::PageDown, KeyCode::PageDown),
            (ct::KeyCode::Insert, KeyCode::Insert),
            (ct::KeyCode::Delete, KeyCode::Delete),
            (ct::KeyCode::F(12), KeyCode::F(12)),
            (ct::KeyCode::Null, KeyCode::Null),
            (ct::KeyCode::Char('é'), KeyCode::Char('é')),
        ];
        for (from, to) in pairs {
            assert_eq!(key(ct_key(from, ct::KeyModifiers::NONE)), KeyEvent::new(to, Modifiers::NONE), "{from:?}");
        }
    }

    #[test]
    fn key_kinds_convert() {
        for (from, to) in [(ct::KeyEventKind::Press, KeyEventKind::Press), (ct::KeyEventKind::Repeat, KeyEventKind::Repeat), (ct::KeyEventKind::Release, KeyEventKind::Release)] {
            let event = Event::Key(ct::KeyEvent::new_with_kind(ct::KeyCode::Char('x'), ct::KeyModifiers::NONE, from));
            assert_eq!(key(event).kind, to);
        }
    }

    #[test]
    fn altgr_characters_keep_ctrl_alt_for_the_wire_conversion() {
        // The Windows console reports AltGr+Q on a German layout as '@' with Ctrl+Alt.
        let altgr = key(ct_key(ct::KeyCode::Char('@'), ct::KeyModifiers::CONTROL | ct::KeyModifiers::ALT));
        assert_eq!(altgr, KeyEvent::new(KeyCode::Char('@'), Modifiers::CONTROL | Modifiers::ALT));
        assert_eq!(input::to_wire(&altgr), Some((Key::Char('@'), 0)));
        assert_eq!(input::paste_char(&altgr), None);
        let ctrl_alt_x = key(ct_key(ct::KeyCode::Char('x'), ct::KeyModifiers::CONTROL | ct::KeyModifiers::ALT));
        assert_eq!(input::to_wire(&ctrl_alt_x), Some((Key::Char('x'), MOD_CTRL | MOD_ALT)));
        let leave = key(ct_key(ct::KeyCode::Char(']'), ct::KeyModifiers::CONTROL));
        assert!(input::is_leave_terminal(&leave));
    }

    #[test]
    fn mouse_kinds_buttons_and_positions_convert() {
        let buttons = [(ct::MouseButton::Left, MouseButton::Left), (ct::MouseButton::Right, MouseButton::Right), (ct::MouseButton::Middle, MouseButton::Middle)];
        for (from, to) in buttons {
            assert_eq!(mouse(ct::MouseEventKind::Down(from), 1, 2, ct::KeyModifiers::NONE).kind, MouseEventKind::Down(to));
            assert_eq!(mouse(ct::MouseEventKind::Up(from), 1, 2, ct::KeyModifiers::NONE).kind, MouseEventKind::Up(to));
            assert_eq!(mouse(ct::MouseEventKind::Drag(from), 1, 2, ct::KeyModifiers::NONE).kind, MouseEventKind::Drag(to));
        }
        let kinds = [
            (ct::MouseEventKind::Moved, MouseEventKind::Moved),
            (ct::MouseEventKind::ScrollUp, MouseEventKind::ScrollUp),
            (ct::MouseEventKind::ScrollDown, MouseEventKind::ScrollDown),
            (ct::MouseEventKind::ScrollLeft, MouseEventKind::ScrollLeft),
            (ct::MouseEventKind::ScrollRight, MouseEventKind::ScrollRight),
        ];
        for (from, to) in kinds {
            assert_eq!(mouse(from, 0, 0, ct::KeyModifiers::NONE).kind, to);
        }
        let event = mouse(ct::MouseEventKind::Down(ct::MouseButton::Left), 123, 45, ct::KeyModifiers::CONTROL | ct::KeyModifiers::SHIFT);
        assert_eq!(event, MouseEvent::at_cell(MouseEventKind::Down(MouseButton::Left), 123, 45, Modifiers::CONTROL | Modifiers::SHIFT));
        assert_eq!((event.column, event.row, event.x, event.y), (123, 45, 123.0, 45.0));
        let wheel = mouse(ct::MouseEventKind::ScrollDown, 9, 8, ct::KeyModifiers::ALT);
        assert_eq!(wheel.modifiers, Modifiers::ALT);
    }

    #[test]
    fn paste_focus_and_resize_convert() {
        assert_eq!(from_crossterm(Event::Paste("a\r\nb".into())), Some(InputEvent::Paste("a\r\nb".into())));
        assert_eq!(from_crossterm(Event::Paste(String::new())), Some(InputEvent::Paste(String::new())), "an empty paste means an image clipboard");
        assert_eq!(from_crossterm(Event::FocusGained), Some(InputEvent::Focus(true)));
        assert_eq!(from_crossterm(Event::FocusLost), Some(InputEvent::Focus(false)));
        assert_eq!(from_crossterm(Event::Resize(120, 40)), Some(InputEvent::Resize(120, 40)));
    }

    #[test]
    fn unsupported_keys_are_ignored() {
        let unsupported = [
            ct::KeyCode::CapsLock,
            ct::KeyCode::ScrollLock,
            ct::KeyCode::NumLock,
            ct::KeyCode::PrintScreen,
            ct::KeyCode::Pause,
            ct::KeyCode::Menu,
            ct::KeyCode::KeypadBegin,
            ct::KeyCode::Media(ct::MediaKeyCode::Play),
            ct::KeyCode::Modifier(ct::ModifierKeyCode::LeftShift),
        ];
        for code in unsupported {
            assert_eq!(from_crossterm(ct_key(code, ct::KeyModifiers::NONE)), None, "{code:?}");
        }
    }

    #[test]
    fn converted_bursts_are_coalesced_into_pastes() {
        let batch = [
            ct_key(ct::KeyCode::Char('a'), ct::KeyModifiers::NONE),
            Event::Key(ct::KeyEvent::new_with_kind(ct::KeyCode::Char('a'), ct::KeyModifiers::NONE, ct::KeyEventKind::Release)),
            ct_key(ct::KeyCode::Enter, ct::KeyModifiers::NONE),
            Event::FocusGained,
            ct_key(ct::KeyCode::Char('B'), ct::KeyModifiers::SHIFT),
        ];
        let events: Vec<InputEvent> = batch.into_iter().filter_map(from_crossterm).collect();
        assert_eq!(input::coalesce(events), vec![InputEvent::Focus(true), InputEvent::Paste("a\nB".into())]);
    }
}
