//! The Sessions panel's RECENT section and the keys that walk it. Viewing a
//! session (a click, Tab, the arrows, a sidebar row, its pane), starting it
//! or typing into it records it as used (`canvas_model::recent`, shared by
//! every window). Ctrl+F6 / Ctrl+Shift+F6 (anywhere, a terminal included)
//! and `]` / `[` (on the canvas and in the sidebar) step to the next older
//! or newer recent session and only view it, as a click does: its live
//! terminal, or its details panel with Start / Resume; nothing starts.
//! Repeated steps keep the order (as Alt+Tab); a second after the last one,
//! or on any other input, the session reached becomes the most recent.

use std::time::{Duration, Instant};

use canvas_model::recent::{self as mru, Walk};

use crate::app::{App, Focus};
use crate::input::{InputEvent, KeyCode, KeyEvent, KeyEventKind, Modifiers, MouseEventKind};
use crate::paths::now_ms;

/// A walk ends this long after its last step.
pub const SETTLE: Duration = Duration::from_millis(1_000);
/// After a click in the sidebar the RECENT rows hold still this long, so a
/// double-click starts the row it began on.
const HOLD: Duration = Duration::from_millis(500);
/// Typing into the most recent session records it again at most this often.
const AGAIN_MS: u64 = 60_000;
/// `UiPrefs::collapsed` entry of a collapsed RECENT section.
pub const COLLAPSED: &str = "@recent";
/// The keys, as the help, the status bar and the README name them.
pub const KEYS: &str = "Ctrl+F6 / Ctrl+Shift+F6";

/// This window's walk and the RECENT rows it shows.
#[derive(Default)]
pub struct RecentState {
    /// A walk in progress and when it last stepped.
    pub walk: Option<(Walk, Instant)>,
    /// The RECENT rows last built (kept while `hold_until` runs).
    pub shown: Vec<String>,
    pub hold_until: Option<Instant>,
    /// The last use recorded and when (keys typed into it need no lookups).
    last: Option<(String, Instant)>,
}

impl App {
    /// Records a use of a box's session (a subagent counts for its
    /// session); not while a walk passes through it.
    pub fn note_access(&mut self, id: &str) {
        if self.recent.walk.is_some() {
            return;
        }
        let Some(session) = self.session_of(id) else { return };
        if self.recent.last.as_ref().is_some_and(|(last, at)| *last == session && at.elapsed() < Duration::from_secs(5)) {
            return;
        }
        self.recent.last = Some((session.clone(), Instant::now()));
        if mru::touch(&mut self.doc, &session, now_ms(), AGAIN_MS) {
            self.mark_dirty();
        }
    }

    /// How many RECENT rows fit: up to `mru::SHOWN`, a quarter of the sidebar.
    pub fn recent_room(&self) -> usize {
        match self.layout.sidebar.height.saturating_sub(3) as usize {
            0 => mru::SHOWN,
            rows => (rows / 4).clamp(3, mru::SHOWN),
        }
    }

    /// The RECENT section's sessions, newest first (held still for a moment
    /// after a click in the sidebar).
    pub fn recent_ids(&self) -> Vec<String> {
        if self.recent.hold_until.is_some() {
            return self.recent.shown.iter().filter(|id| self.doc.node(id).is_some()).cloned().collect();
        }
        mru::order(&self.doc).into_iter().take(self.recent_room()).map(|n| n.id.clone()).collect()
    }

    /// A click in the sidebar: the RECENT rows hold still for a double-click.
    pub fn hold_recent(&mut self) {
        self.recent.hold_until = Some(Instant::now() + HOLD);
    }

    /// Whether a key walks the recent sessions: `Some(older)`. Ctrl+F6 and
    /// Ctrl+Shift+F6 everywhere (terminals pass every other key to the
    /// agent); `]` and `[` (AltGr ones too) outside terminals and the filter.
    pub fn recent_key(&self, key: &KeyEvent) -> Option<bool> {
        let mods = key.modifiers;
        let rest = mods.difference(Modifiers::SHIFT);
        match key.code {
            KeyCode::F(6) if mods.contains(Modifiers::CONTROL) && !mods.contains(Modifiers::ALT) => Some(!mods.contains(Modifiers::SHIFT)),
            KeyCode::Char(c @ (']' | '['))
                if self.focus != Focus::Terminal && !self.side_filter_active && (rest.is_empty() || rest == Modifiers::CONTROL | Modifiers::ALT) =>
            {
                Some(c == ']')
            }
            _ => None,
        }
    }

    /// One step through the recent sessions (`older`: the one used before
    /// it): selects the session, flies the canvas to it and shows it as a
    /// click would (its live terminal with the keyboard, else its details
    /// panel); never starts it. The list keeps its order until the walk ends.
    pub fn recent_step(&mut self, older: bool) {
        let mut walk = match self.recent.walk.take() {
            Some((walk, _)) => walk,
            None => {
                let shown = self.selection.last().and_then(|id| self.session_of(id));
                let current = shown.or_else(|| self.focused_term.clone().filter(|_| self.focus == Focus::Terminal));
                Walk::new(self.recent_ids(), current.as_deref())
            }
        };
        let total = walk.order.len();
        let mut target = None;
        for _ in 0..total {
            match walk.step(older) {
                // A box another window removed meanwhile is passed over.
                Some(id) if self.doc.node(id).is_some() => {
                    target = Some(id.to_owned());
                    break;
                }
                Some(_) => {}
                None => break,
            }
        }
        let Some(id) = target else {
            return self.notify(format!("No recent sessions yet: viewing, starting or typing into one adds it ({KEYS} walk them)."), false);
        };
        let place = walk.at.unwrap_or(0) + 1;
        self.recent.walk = Some((walk, Instant::now()));
        self.reveal_node(&id);
        self.view_node(&id);
        let running = self.owned_alive(&id) || self.launching.contains_key(&id);
        if !running && self.focus == Focus::Terminal {
            // Its details panel shows; Enter on the canvas starts it.
            self.set_focus(Focus::Canvas);
        }
        let name = self.doc.node(&id).map(|n| n.name.clone()).unwrap_or_default();
        let then = if running { "" } else { " · Enter starts it" };
        self.notify(format!("Recent {place} of {total}: {name}{then} · Ctrl+F6 older · Ctrl+Shift+F6 newer"), false);
    }

    /// Ends a walk: the session it reached becomes the most recent.
    pub fn settle_recent(&mut self) {
        if let Some((walk, _)) = self.recent.walk.take()
            && walk.moved
            && let Some(id) = walk.current().map(str::to_owned)
        {
            self.note_access(&id);
        }
    }

    /// Before an input event: anything that acts (keys other than the walk
    /// keys, clicks, the wheel, text) ends a walk first.
    pub fn recent_input(&mut self, event: &InputEvent) {
        if self.recent.walk.is_none() {
            return;
        }
        let acts = match event {
            InputEvent::Key(key) => key.kind != KeyEventKind::Release && !(self.modals.is_empty() && self.recent_key(key).is_some()),
            InputEvent::Mouse(mouse) => mouse.kind != MouseEventKind::Moved,
            InputEvent::Resize(..) | InputEvent::Focus(_) => false,
            InputEvent::Paste(_) | InputEvent::Text(_) => true,
        };
        if acts {
            self.settle_recent();
        }
    }

    /// Every tick: a walk a second old ends; held RECENT rows move again.
    pub fn tick_recent(&mut self) {
        if self.recent.walk.as_ref().is_some_and(|(_, at)| at.elapsed() >= SETTLE) {
            self.settle_recent();
            self.needs_draw = true;
        }
        if self.recent.hold_until.is_some_and(|until| Instant::now() >= until) {
            self.recent.hold_until = None;
            self.needs_draw = true;
        }
    }

    /// A walk or a held RECENT list is waiting for its time (front ends tick quickly).
    pub fn recent_pending(&self) -> bool {
        self.recent.walk.is_some() || self.recent.hold_until.is_some()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossbeam_channel::{Receiver, bounded, unbounded};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    use super::*;
    use crate::app::SideKind;
    use crate::input::{MouseButton, MouseEvent};
    use crate::protocol::{ClientMsg, TermInfo};
    use crate::side::RightDetail;

    fn app(ids: &[&str]) -> (App, Receiver<ClientMsg>) {
        let nodes = ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let mut node: crate::doc::Node = serde_json::from_value(serde_json::json!({ "id": id, "kind": "session", "name": format!("Session {id}"), "x": 2, "y": 2 + i * 7, "reviewed_ms": 1, "cwd": "." })).unwrap();
                node.command = Some(vec!["powershell.exe".into()]);
                node.fresh = true;
                node
            })
            .collect();
        let (tx, sent) = bounded(1024);
        let (_service, service_rx) = bounded(1);
        let (app_tx, _app_rx) = unbounded();
        let doc = crate::doc::Doc { nodes, ..Default::default() };
        let mut app = App::new(crate::conn::Conn::detached(tx, service_rx), Arc::new(crate::rpc::Offline), crate::sync::Opened::detached(doc), true, ".".into(), app_tx, None);
        app.selection.clear();
        (app, sent)
    }

    fn launches(sent: &Receiver<ClientMsg>) -> Vec<String> {
        sent.try_iter().filter_map(|m| if let ClientMsg::Launch { id, .. } = m { Some(id) } else { None }).collect()
    }

    fn order(app: &App) -> Vec<String> {
        mru::order(&app.doc).into_iter().map(|n| n.id.clone()).collect()
    }

    fn press(app: &mut App, code: KeyCode, mods: Modifiers) {
        app.on_input(vec![InputEvent::Key(KeyEvent::new(code, mods))]);
    }

    /// Views a box as a click does (each view recorded, however quick).
    fn view(app: &mut App, id: &str) {
        app.recent.last = None;
        app.view_node(id);
    }

    #[test]
    fn the_keys_walk_recent_sessions_without_starting_them_and_the_list_reorders_when_the_walk_ends() {
        let (mut app, sent) = app(&["a", "b", "c", "d"]);
        for id in ["a", "b", "c"] {
            view(&mut app, id);
        }
        assert_eq!(order(&app), ["c", "b", "a"], "viewing records use, newest first");
        // Ctrl+F6 walks to the one used before, again to the one before that; the order holds.
        press(&mut app, KeyCode::F(6), Modifiers::CONTROL);
        assert_eq!(app.selection, ["b"]);
        press(&mut app, KeyCode::F(6), Modifiers::CONTROL);
        assert_eq!((app.selection.clone(), app.right_detail.clone()), (vec!["a".to_owned()], Some(RightDetail::Session("a".into()))), "its details panel");
        assert_eq!(order(&app), ["c", "b", "a"], "a walk does not reorder");
        // Ctrl+Shift+F6 goes back.
        press(&mut app, KeyCode::F(6), Modifiers::CONTROL | Modifiers::SHIFT);
        assert_eq!(app.selection, ["b"]);
        assert!(launches(&sent).is_empty(), "walking never starts a session");
        // A second after the last step the session reached becomes the most recent.
        app.recent.walk.as_mut().unwrap().1 -= SETTLE;
        app.tick_recent();
        assert!(app.recent.walk.is_none());
        assert_eq!(order(&app), ["b", "c", "a"]);
        // `]` on the canvas walks too; any other key ends the walk at once.
        app.recent.last = None;
        press(&mut app, KeyCode::Char(']'), Modifiers::NONE);
        assert_eq!(app.selection, ["c"]);
        press(&mut app, KeyCode::Char('['), Modifiers::NONE);
        press(&mut app, KeyCode::Char('['), Modifiers::NONE);
        assert_eq!(app.selection, ["a"], "[ goes back, round to the oldest");
        press(&mut app, KeyCode::Esc, Modifiers::NONE);
        assert_eq!(order(&app), ["a", "b", "c"]);
        assert!(launches(&sent).is_empty());
        // Starting a session and typing into it count as using it.
        app.start_node("d");
        assert_eq!(launches(&sent), ["d"]);
        assert_eq!(order(&app)[0], "d");
        app.terms.insert("c".into(), TermInfo { id: "c".into(), alive: true, ..Default::default() });
        app.recent.last = None;
        app.send(ClientMsg::Input { id: "c".into(), input: crate::protocol::Input::Key { key: crate::protocol::Key::Char('x'), mods: 0 } });
        assert_eq!(order(&app)[0], "c", "typing records use");
        // A box removed in another window is passed over.
        app.doc.remove_node("d");
        assert!(!order(&app).contains(&"d".to_owned()));
    }

    #[test]
    fn in_a_terminal_ctrl_f6_walks_and_brackets_reach_the_agent() {
        let (mut app, sent) = app(&["a", "b", "c"]);
        for id in ["a", "b"] {
            view(&mut app, id);
        }
        for id in ["b", "c"] {
            app.terms.insert(id.into(), TermInfo { id: id.into(), alive: true, ..Default::default() });
        }
        view(&mut app, "c");
        assert_eq!(app.focus, Focus::Terminal, "a running session's terminal gets the keyboard");
        let _ = launches(&sent);
        press(&mut app, KeyCode::Char(']'), Modifiers::NONE);
        assert_eq!(app.selection, ["c"], "] is the agent's in a terminal");
        assert!(sent.try_iter().any(|m| matches!(m, ClientMsg::Input { id, .. } if id == "c")));
        press(&mut app, KeyCode::F(6), Modifiers::CONTROL);
        assert_eq!((app.selection.clone(), app.focus, app.focused_term.clone()), (vec!["b".to_owned()], Focus::Terminal, Some("b".to_owned())), "a running one: its terminal with the keyboard");
        press(&mut app, KeyCode::F(6), Modifiers::CONTROL);
        assert_eq!((app.selection.clone(), app.focus), (vec!["a".to_owned()], Focus::Canvas), "a stopped one: its details, the keyboard on the canvas");
        assert!(launches(&sent).is_empty());
        // Ctrl+] still leaves a terminal and F6 still cycles the focus.
        assert_eq!(app.recent_key(&KeyEvent::new(KeyCode::Char(']'), Modifiers::CONTROL)), None);
        assert_eq!(app.recent_key(&KeyEvent::new(KeyCode::F(6), Modifiers::NONE)), None);
        assert_eq!(app.recent_key(&KeyEvent::new(KeyCode::Char(']'), Modifiers::CONTROL | Modifiers::ALT)), Some(true), "AltGr+9 on a German keyboard");
    }

    #[test]
    fn the_recent_section_lists_sessions_and_holds_still_for_a_double_click() {
        let (mut app, sent) = app(&["a", "b", "c"]);
        let area = Rect::new(0, 0, 150, 40);
        let mut buf = Buffer::empty(area);
        crate::ui::render(&mut app, &mut buf, area, None);
        assert!(!app.side_rows.iter().any(|r| r.kind == SideKind::RecentHeader), "nothing used yet: no section");
        for id in ["a", "b", "c"] {
            view(&mut app, id);
        }
        app.set_focus(Focus::Canvas);
        crate::ui::render(&mut app, &mut buf, area, None);
        let kinds: Vec<SideKind> = app.side_rows.iter().map(|r| r.kind.clone()).take(5).collect();
        assert_eq!(kinds, [SideKind::RecentHeader, SideKind::Recent("c".into()), SideKind::Recent("b".into()), SideKind::Recent("a".into()), SideKind::Section]);
        // Two clicks on the oldest row: the first views it (it becomes the newest), yet
        // the rows hold still, so the second click starts that same session.
        let index = app.side_rows.iter().position(|r| r.kind == SideKind::Recent("a".into())).unwrap();
        let y = app.layout.sidebar_rows.iter().find(|(_, i)| *i == index).map(|(y, _)| *y).unwrap();
        let x = app.layout.sidebar.x + 4;
        let click = |app: &mut App| {
            app.on_mouse(MouseEvent::at_cell(MouseEventKind::Down(MouseButton::Left), x, y, Modifiers::NONE));
            app.on_mouse(MouseEvent::at_cell(MouseEventKind::Up(MouseButton::Left), x, y, Modifiers::NONE));
        };
        click(&mut app);
        assert_eq!((app.selection.clone(), order(&app)[0].clone()), (vec!["a".to_owned()], "a".to_owned()));
        assert!(launches(&sent).is_empty(), "a click only views");
        crate::ui::render(&mut app, &mut buf, area, None);
        assert_eq!(app.side_rows[index].kind, SideKind::Recent("a".into()), "held still");
        assert_eq!(app.side_sel, index, "the highlight stays on the RECENT row");
        click(&mut app);
        assert_eq!(launches(&sent), ["a"], "the double-click started the row it began on");
        // Once the hold ends the list shows the new order.
        app.recent.hold_until = Some(Instant::now());
        app.tick_recent();
        crate::ui::render(&mut app, &mut buf, area, None);
        assert_eq!(app.side_rows[1].kind, SideKind::Recent("a".into()));
        // The header folds the section.
        let header = app.layout.sidebar_rows.iter().find(|(_, i)| *i == 0).map(|(y, _)| *y).unwrap();
        app.on_mouse(MouseEvent::at_cell(MouseEventKind::Down(MouseButton::Left), x, header, Modifiers::NONE));
        crate::ui::render(&mut app, &mut buf, area, None);
        assert!(app.side_rows[0].collapsed && app.side_rows[1].kind == SideKind::Section);
    }
}
