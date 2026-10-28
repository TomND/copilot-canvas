//! winit input → canvas-core `InputEvent`s (keys with their text, IME text,
//! mouse at fractional cell positions, wheel notches).

use canvas_core::input::{InputEvent, KeyCode, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseEventKind};
use winit::event::{ElementState, MouseScrollDelta};
use winit::keyboard::{Key, ModifiersState, NamedKey};

pub fn modifiers(state: ModifiersState) -> Modifiers {
    let mut m = Modifiers::NONE;
    if state.shift_key() {
        m |= Modifiers::SHIFT;
    }
    if state.control_key() {
        m |= Modifiers::CONTROL;
    }
    if state.alt_key() {
        m |= Modifiers::ALT;
    }
    if state.super_key() {
        m |= Modifiers::SUPER;
    }
    m
}

fn named(key: NamedKey, shift: bool) -> Option<KeyCode> {
    Some(match key {
        NamedKey::Enter => KeyCode::Enter,
        NamedKey::Tab if shift => KeyCode::BackTab,
        NamedKey::Tab => KeyCode::Tab,
        NamedKey::Backspace => KeyCode::Backspace,
        NamedKey::Escape => KeyCode::Esc,
        NamedKey::ArrowUp => KeyCode::Up,
        NamedKey::ArrowDown => KeyCode::Down,
        NamedKey::ArrowLeft => KeyCode::Left,
        NamedKey::ArrowRight => KeyCode::Right,
        NamedKey::Home => KeyCode::Home,
        NamedKey::End => KeyCode::End,
        NamedKey::PageUp => KeyCode::PageUp,
        NamedKey::PageDown => KeyCode::PageDown,
        NamedKey::Insert => KeyCode::Insert,
        NamedKey::Delete => KeyCode::Delete,
        NamedKey::Space => KeyCode::Char(' '),
        NamedKey::F1 => KeyCode::F(1),
        NamedKey::F2 => KeyCode::F(2),
        NamedKey::F3 => KeyCode::F(3),
        NamedKey::F4 => KeyCode::F(4),
        NamedKey::F5 => KeyCode::F(5),
        NamedKey::F6 => KeyCode::F(6),
        NamedKey::F7 => KeyCode::F(7),
        NamedKey::F8 => KeyCode::F(8),
        NamedKey::F9 => KeyCode::F(9),
        NamedKey::F10 => KeyCode::F(10),
        NamedKey::F11 => KeyCode::F(11),
        NamedKey::F12 => KeyCode::F(12),
        NamedKey::F13 => KeyCode::F(13),
        NamedKey::F14 => KeyCode::F(14),
        NamedKey::F15 => KeyCode::F(15),
        NamedKey::F16 => KeyCode::F(16),
        NamedKey::F17 => KeyCode::F(17),
        NamedKey::F18 => KeyCode::F(18),
        NamedKey::F19 => KeyCode::F(19),
        NamedKey::F20 => KeyCode::F(20),
        NamedKey::F21 => KeyCode::F(21),
        NamedKey::F22 => KeyCode::F(22),
        NamedKey::F23 => KeyCode::F(23),
        NamedKey::F24 => KeyCode::F(24),
        _ => return None,
    })
}

/// A key press as the core sees it. `logical` is the key with every
/// modifier but Ctrl applied (AltGr layouts already composed: winit drops
/// Ctrl and Alt for AltGr); text of several characters is typed as text.
pub fn key(logical: &Key, state: ElementState, mods: ModifiersState) -> Option<InputEvent> {
    if state == ElementState::Released {
        return None;
    }
    let m = modifiers(mods);
    let code = match logical {
        Key::Named(key) => named(*key, mods.shift_key())?,
        Key::Character(text) => {
            let mut chars = text.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => KeyCode::Char(c),
                (Some(_), Some(_)) if !mods.control_key() && !mods.alt_key() => return Some(InputEvent::Text(text.to_string())),
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(InputEvent::Key(KeyEvent::new(code, m)))
}

pub fn button(button: winit::event::MouseButton) -> Option<MouseButton> {
    match button {
        winit::event::MouseButton::Left => Some(MouseButton::Left),
        winit::event::MouseButton::Right => Some(MouseButton::Right),
        winit::event::MouseButton::Middle => Some(MouseButton::Middle),
        _ => None,
    }
}

/// A mouse event at a position in fractional cells.
pub fn mouse(kind: MouseEventKind, (x, y): (f32, f32), mods: ModifiersState) -> InputEvent {
    InputEvent::Mouse(MouseEvent::at(kind, x, y, modifiers(mods)))
}

/// Accumulates wheel movement into notches (a pixel delta of three rows is one).
#[derive(Default)]
pub struct Wheel {
    x: f32,
    y: f32,
}

impl Wheel {
    pub fn notches(&mut self, delta: MouseScrollDelta, row_px: f32) -> Vec<MouseEventKind> {
        let (dx, dy) = match delta {
            MouseScrollDelta::LineDelta(x, y) => (x, y),
            MouseScrollDelta::PixelDelta(p) => {
                let notch = (row_px * 3.0).max(1.0);
                (p.x as f32 / notch, p.y as f32 / notch)
            }
        };
        // A change of direction starts afresh.
        if self.x * dx < 0.0 {
            self.x = 0.0;
        }
        if self.y * dy < 0.0 {
            self.y = 0.0;
        }
        self.x += dx;
        self.y += dy;
        let mut out = Vec::new();
        while self.y >= 1.0 {
            self.y -= 1.0;
            out.push(MouseEventKind::ScrollUp);
        }
        while self.y <= -1.0 {
            self.y += 1.0;
            out.push(MouseEventKind::ScrollDown);
        }
        while self.x >= 1.0 {
            self.x -= 1.0;
            out.push(MouseEventKind::ScrollLeft);
        }
        while self.x <= -1.0 {
            self.x += 1.0;
            out.push(MouseEventKind::ScrollRight);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::dpi::PhysicalPosition;

    #[test]
    fn keys_keep_their_modifiers_and_text() {
        let ctrl = ModifiersState::CONTROL;
        assert_eq!(key(&Key::Character("c".into()), ElementState::Pressed, ctrl), Some(InputEvent::Key(KeyEvent::new(KeyCode::Char('c'), Modifiers::CONTROL))));
        assert_eq!(key(&Key::Character("]".into()), ElementState::Pressed, ctrl), Some(InputEvent::Key(KeyEvent::new(KeyCode::Char(']'), Modifiers::CONTROL))));
        assert_eq!(key(&Key::Named(NamedKey::Tab), ElementState::Pressed, ModifiersState::SHIFT), Some(InputEvent::Key(KeyEvent::new(KeyCode::BackTab, Modifiers::SHIFT))));
        assert_eq!(key(&Key::Character("@".into()), ElementState::Pressed, ModifiersState::empty()), Some(InputEvent::Key(KeyEvent::new(KeyCode::Char('@'), Modifiers::NONE))), "AltGr text arrives composed");
        assert_eq!(key(&Key::Character("ab".into()), ElementState::Pressed, ModifiersState::empty()), Some(InputEvent::Text("ab".into())));
        assert_eq!(key(&Key::Character("a".into()), ElementState::Released, ModifiersState::empty()), None);
        assert_eq!(key(&Key::Named(NamedKey::Space), ElementState::Pressed, ModifiersState::empty()), Some(InputEvent::Key(KeyEvent::new(KeyCode::Char(' '), Modifiers::NONE))));
    }

    #[test]
    fn wheel_pixels_accumulate_into_notches() {
        let mut wheel = Wheel::default();
        assert!(wheel.notches(MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 20.0)), 18.0).is_empty());
        assert_eq!(wheel.notches(MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 40.0)), 18.0), [MouseEventKind::ScrollUp]);
        assert_eq!(wheel.notches(MouseScrollDelta::LineDelta(0.0, -2.0), 18.0), [MouseEventKind::ScrollDown, MouseEventKind::ScrollDown]);
    }
}
