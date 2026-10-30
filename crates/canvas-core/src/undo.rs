//! Undo and redo on the canvas, one history per window (every `App`): a
//! single chronological stack, so Ctrl+Z always takes back the most recent
//! change of this window, whichever kind it was:
//!
//! - **layout steps**: one completed gesture each: a drag of boxes (with
//!   their subagent boxes and stacks), of a stack or of a group (with
//!   everything that moved with it, nested groups too), a group's ◢ resize,
//!   *Fit to members*, and a burst of arrow-key moves of one selection or
//!   group (presses less than [`BURST`] apart). A step keeps the exact
//!   place (and a group's size) of every item it changed, before and after;
//! - **removals** (the boxes and the subsession links they cut);
//! - **VS Code syncs** (the canvas before the sync).
//!
//! Undo applies as an ordinary document edit, so other windows and a remote
//! service see it. A layout undo only touches positions and sizes: it never
//! starts, stops or changes a session, and never brings back a removed box
//! (removed items are skipped; so are items another window or client moved
//! since, which keep their new place). Ctrl+Y (or Ctrl+Shift+Z) redoes undone
//! layout steps; removals and syncs are not redone, and undoing one clears
//! the redo list. A new change clears it too. Keys in a terminal and in text
//! fields never reach this history (`events.rs`).

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::app::{App, Drag};
use crate::doc::{Doc, Node};

/// Steps kept per window (the oldest go first).
pub const LIMIT: usize = 100;
/// Arrow-key moves of one target this close together are one step.
pub const BURST: Duration = Duration::from_millis(1000);

/// A group's place and size (world units).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Frame {
    fn of(group: &crate::doc::Group) -> Self {
        Self { x: group.x, y: group.y, w: group.w, h: group.h }
    }
}

/// What a layout step did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutKind {
    /// Boxes, a stack or a group moved (mouse or arrow keys).
    Move,
    /// A group resized with its ◢ corner.
    Resize,
    /// A group fitted to its members.
    Fit,
}

impl LayoutKind {
    fn noun(self) -> &'static str {
        match self {
            LayoutKind::Move => "move",
            LayoutKind::Resize => "resize",
            LayoutKind::Fit => "fit",
        }
    }
}

/// A box's move in a step: its id, its place before and after.
pub type NodeMove = (String, (i32, i32), (i32, i32));

/// One completed layout gesture: each box (id, place before, place after)
/// and group (id, frame before, frame after) it changed.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutStep {
    pub kind: LayoutKind,
    pub nodes: Vec<NodeMove>,
    pub groups: Vec<(String, Frame, Frame)>,
}

/// One step Ctrl+Z takes back.
#[derive(Clone, Debug)]
pub enum Undo {
    /// The canvas before a bulk change (a VS Code sync): label, document.
    Doc(String, Box<Doc>),
    /// Boxes removed together, and the subsession links (child, parent)
    /// their removal cut.
    Removed { nodes: Vec<Node>, links: Vec<(String, String)> },
    /// Boxes and groups moved or resized by one gesture.
    Layout(LayoutStep),
}

/// Who a gesture belongs to: the mouse (until the button comes up), one
/// action, or a burst of arrow keys for one target (its key).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Mouse,
    Once,
    Keys(String),
}

/// A gesture in progress: the first-seen place of everything it touched.
struct Gesture {
    kind: LayoutKind,
    source: Source,
    last: Instant,
    nodes: HashMap<String, (i32, i32)>,
    groups: HashMap<String, Frame>,
}

/// This window's history.
#[derive(Default)]
pub struct History {
    steps: VecDeque<Undo>,
    redo: Vec<LayoutStep>,
    gesture: Option<Gesture>,
}

impl History {
    /// The step Ctrl+Z takes back next.
    pub fn last(&self) -> Option<&Undo> {
        self.steps.back()
    }

    pub fn can_undo(&self) -> bool {
        !self.steps.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Steps kept.
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// A gesture is recording (not yet a step).
    pub fn recording(&self) -> bool {
        self.gesture.is_some()
    }

    fn push(&mut self, step: Undo) {
        self.steps.push_back(step);
        while self.steps.len() > LIMIT {
            self.steps.pop_front();
        }
    }
}

/// What applying a layout step did.
#[derive(Default)]
struct Applied {
    restored: usize,
    gone: usize,
    elsewhere: usize,
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

impl App {
    /// Records a change for Ctrl+Z (a removal or a sync); the layout gesture
    /// in progress becomes a step first, and nothing is left to redo.
    pub fn record_undo(&mut self, step: Undo) {
        self.commit_layout();
        self.undo.redo.clear();
        self.undo.push(step);
    }

    /// The gesture `kind` from `source`: the one in progress when it is the
    /// same (a key burst within [`BURST`]), else a new one.
    fn gesture(&mut self, kind: LayoutKind, source: Source) -> &mut Gesture {
        let same = self.undo.gesture.as_ref().is_some_and(|g| g.kind == kind && g.source == source && (!matches!(source, Source::Keys(_)) || g.last.elapsed() < BURST));
        if !same {
            self.commit_layout();
            self.undo.gesture = Some(Gesture { kind, source, last: Instant::now(), nodes: HashMap::new(), groups: HashMap::new() });
        }
        let gesture = self.undo.gesture.as_mut().expect("just set");
        gesture.last = Instant::now();
        gesture
    }

    /// Notes the place of boxes and groups a gesture is about to change (the
    /// first time it touches each).
    fn note_layout(&mut self, kind: LayoutKind, source: Source, nodes: &[String], groups: &[String]) {
        let places: Vec<(String, (i32, i32))> = nodes.iter().filter_map(|id| self.doc.node(id).map(|n| (id.clone(), (n.x, n.y)))).collect();
        let frames: Vec<(String, Frame)> = groups.iter().filter_map(|id| self.doc.group(id).map(|g| (id.clone(), Frame::of(g)))).collect();
        let gesture = self.gesture(kind, source);
        for (id, place) in places {
            gesture.nodes.entry(id).or_insert(place);
        }
        for (id, frame) in frames {
            gesture.groups.entry(id).or_insert(frame);
        }
    }

    /// Moves boxes (their subagent boxes and stacks with them, each once)
    /// as part of a gesture.
    pub fn move_nodes_recorded(&mut self, ids: &[String], dx: i32, dy: i32, source: Source) {
        if dx == 0 && dy == 0 {
            return;
        }
        let moving = self.doc.nodes_moving_with(ids);
        self.note_layout(LayoutKind::Move, source, &moving, &[]);
        self.doc.move_nodes(ids, dx, dy);
    }

    /// Moves a group with everything in it as part of a gesture.
    pub fn move_group_recorded(&mut self, id: &str, dx: i32, dy: i32, source: Source) {
        if dx == 0 && dy == 0 {
            return;
        }
        let (nodes, groups) = self.doc.group_moving_with(id);
        self.note_layout(LayoutKind::Move, source, &nodes, &groups);
        self.doc.move_group(id, dx, dy);
    }

    /// Sizes a group (its ◢ corner) as part of a gesture.
    pub fn resize_group_recorded(&mut self, id: &str, w: i32, h: i32) {
        let w = w.max(crate::doc::GROUP_MIN_W);
        let h = h.max(crate::doc::GROUP_MIN_H);
        if self.doc.group(id).is_none_or(|g| (g.w, g.h) == (w, h)) {
            return;
        }
        self.note_layout(LayoutKind::Resize, Source::Mouse, &[], &[id.to_owned()]);
        if let Some(group) = self.doc.group_mut(id) {
            group.w = w;
            group.h = h;
        }
    }

    /// *Fit to members*: one step of its own.
    pub fn fit_group_recorded(&mut self, id: &str) {
        self.note_layout(LayoutKind::Fit, Source::Once, &[], &[id.to_owned()]);
        self.doc.fit_group(id);
        self.commit_layout();
    }

    /// The gesture in progress becomes one undo step (when it changed
    /// anything), with a hint that Ctrl+Z takes it back.
    pub fn commit_layout(&mut self) {
        let Some(gesture) = self.undo.gesture.take() else { return };
        let mut nodes: Vec<NodeMove> = gesture
            .nodes
            .into_iter()
            .filter_map(|(id, before)| {
                let n = self.doc.node(&id)?;
                ((n.x, n.y) != before).then_some((id, before, (n.x, n.y)))
            })
            .collect();
        let mut groups: Vec<(String, Frame, Frame)> = gesture
            .groups
            .into_iter()
            .filter_map(|(id, before)| {
                let after = Frame::of(self.doc.group(&id)?);
                (after != before).then_some((id, before, after))
            })
            .collect();
        if nodes.is_empty() && groups.is_empty() {
            return;
        }
        nodes.sort_by(|a, b| a.0.cmp(&b.0));
        groups.sort_by(|a, b| a.0.cmp(&b.0));
        let step = LayoutStep { kind: gesture.kind, nodes, groups };
        let hint = format!("{} · Ctrl+Z undo {}", self.layout_summary(&step), step.kind.noun());
        self.undo.redo.clear();
        self.undo.push(Undo::Layout(step));
        self.notify(hint, false);
    }

    /// "Moved “Name”", "Moved 3 boxes", "Moved group “G” (4 boxes)", "Resized group “G”".
    fn layout_summary(&self, step: &LayoutStep) -> String {
        // The outermost group (nested ones move with it).
        let group = step.groups.iter().max_by_key(|(_, _, after)| after.w as i64 * after.h as i64).map(|(id, _, _)| self.doc.group(id).map(|g| g.name.clone()).unwrap_or_else(|| id.clone()));
        let node = step.nodes.first().map(|(id, _, _)| self.doc.node(id).map(|n| n.name.clone()).unwrap_or_else(|| id.clone()));
        match (step.kind, group, node) {
            (LayoutKind::Resize, Some(group), _) => format!("Resized group “{group}”"),
            (LayoutKind::Fit, Some(group), _) => format!("Fitted group “{group}” to its members"),
            (_, Some(group), _) => format!("Moved group “{group}” ({})", plural(step.nodes.len(), "box", "boxes")),
            (_, None, Some(node)) if step.nodes.len() == 1 => format!("Moved “{node}”"),
            _ => format!("Moved {}", plural(step.nodes.len(), "box", "boxes")),
        }
    }

    /// Ends a burst of arrow-key moves (another key came).
    pub(crate) fn end_key_burst(&mut self) {
        if self.undo.gesture.as_ref().is_some_and(|g| matches!(g.source, Source::Keys(_))) {
            self.commit_layout();
        }
    }

    /// Ends a key burst or a mouse gesture whose button came up elsewhere.
    pub(crate) fn tick_undo(&mut self) {
        let due = match &self.undo.gesture {
            Some(g) => match g.source {
                Source::Keys(_) => g.last.elapsed() >= BURST,
                Source::Mouse | Source::Once => !matches!(self.drag, Some(Drag::Nodes { .. } | Drag::Group { .. } | Drag::Resize { .. })),
            },
            None => false,
        };
        if due {
            self.commit_layout();
        }
    }

    /// A drag of boxes or groups in progress ends where it is (before an undo or redo).
    fn end_canvas_drag(&mut self) {
        if matches!(self.drag, Some(Drag::Nodes { .. } | Drag::Group { .. } | Drag::Resize { .. })) {
            self.drag = None;
        }
        self.commit_layout();
    }

    /// Ctrl+Z: takes back this window's most recent change.
    pub fn undo_last(&mut self) {
        self.end_canvas_drag();
        match self.undo.steps.pop_back() {
            None => self.notify("Nothing to undo.", false),
            Some(Undo::Layout(step)) => {
                let applied = self.apply_layout(&step, false);
                let what = self.layout_summary(&step);
                self.undo.redo.push(step);
                self.layout_notice("Undid", &what, &applied, "Ctrl+Y redoes it");
            }
            Some(Undo::Removed { nodes, links }) => {
                self.undo.redo.clear();
                self.restore_removed(nodes, links);
            }
            Some(Undo::Doc(label, doc)) => {
                self.undo.redo.clear();
                self.restore_doc(label, *doc);
            }
        }
    }

    /// Ctrl+Y / Ctrl+Shift+Z: does an undone layout step again.
    pub fn redo_last(&mut self) {
        self.end_canvas_drag();
        let Some(step) = self.undo.redo.pop() else { return self.notify("Nothing to redo.", false) };
        let applied = self.apply_layout(&step, true);
        let what = self.layout_summary(&step);
        self.undo.push(Undo::Layout(step));
        self.layout_notice("Redid", &what, &applied, "Ctrl+Z undoes it");
    }

    fn layout_notice(&mut self, verb: &str, what: &str, applied: &Applied, then: &str) {
        // "Undid: moved 3 boxes" reads better with the summary in lower case.
        let what = what.char_indices().next().map(|(_, c)| format!("{}{}", c.to_lowercase(), &what[c.len_utf8()..])).unwrap_or_default();
        let mut skipped = Vec::new();
        if applied.gone > 0 {
            skipped.push(format!("{} removed since", plural(applied.gone, "item", "items")));
        }
        if applied.elsewhere > 0 {
            skipped.push(format!("{} moved elsewhere since", plural(applied.elsewhere, "item", "items")));
        }
        let text = match (applied.restored, skipped.is_empty()) {
            (_, true) => format!("{verb}: {what} · {then}"),
            (0, false) => format!("{verb} nothing ({what}): {} (left as they are).", skipped.join(", ")),
            (n, false) => format!("{verb}: {what}, {} restored; {} (left as they are).", plural(n, "item", "items"), skipped.join(", ")),
        };
        self.mark_dirty();
        self.notify(text, false);
    }

    /// Puts a step's items at their places before (`forward`: after) it,
    /// those still where it left them; removed ones are never brought back.
    fn apply_layout(&mut self, step: &LayoutStep, forward: bool) -> Applied {
        let mut applied = Applied::default();
        for (id, before, after) in &step.nodes {
            let (from, to) = if forward { (*before, *after) } else { (*after, *before) };
            match self.doc.node_mut(id) {
                None => applied.gone += 1,
                Some(node) if (node.x, node.y) != from => applied.elsewhere += 1,
                Some(node) => {
                    (node.x, node.y) = to;
                    applied.restored += 1;
                }
            }
        }
        for (id, before, after) in &step.groups {
            let (from, to) = if forward { (*before, *after) } else { (*after, *before) };
            match self.doc.group_mut(id) {
                None => applied.gone += 1,
                Some(group) if Frame::of(group) != from => applied.elsewhere += 1,
                Some(group) => {
                    (group.x, group.y, group.w, group.h) = (to.x, to.y, to.w, to.h);
                    applied.restored += 1;
                }
            }
        }
        applied
    }

    /// The label of the canvas menu's undo item.
    pub fn undo_label(&self) -> &'static str {
        match self.undo.last() {
            Some(Undo::Removed { .. }) => "Undo remove",
            Some(Undo::Doc(..)) => "Undo last sync",
            Some(Undo::Layout(step)) => match step.kind {
                LayoutKind::Move => "Undo move",
                LayoutKind::Resize => "Undo resize",
                LayoutKind::Fit => "Undo fit",
            },
            None => "Undo",
        }
    }
}

/// A canvas for the layout and selection tests, and the gestures they use.
#[cfg(test)]
pub(crate) mod fixture {
    use std::sync::Arc;

    use crossbeam_channel::{Receiver, bounded, unbounded};

    use crate::app::App;
    use crate::conn::Conn;
    use crate::doc::{Doc, Group, Node, NodeKind};
    use crate::input::{InputEvent, KeyCode, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseEventKind};
    use crate::protocol::ClientMsg;
    use crate::sync::Opened;

    pub fn node(id: &str, kind: NodeKind, parent: Option<&str>, x: i32, y: i32) -> Node {
        let mut node: Node = serde_json::from_value(serde_json::json!({ "id": id, "kind": "session", "name": id, "x": x, "y": y, "reviewed_ms": 1, "cwd": "." })).unwrap();
        node.kind = kind;
        node.parent = parent.map(str::to_owned);
        if kind == NodeKind::Agent {
            node.agent_key = Some("k".into());
        }
        node
    }

    /// Group "g" (0,0 110×30) holds a parent `p` with its subagent box
    /// `p:helper`, a subsession `c` with its subagent box `c:x` (both in the
    /// nested group "g-in"), and `o`; `z` is outside. The view starts at the
    /// world origin on a 200×60 canvas (100 %: a cell is a world unit).
    pub fn doc() -> Doc {
        let nodes = vec![
            node("p", NodeKind::Session, None, 2, 2),
            node("p:helper", NodeKind::Agent, Some("p"), 6, 8),
            node("c", NodeKind::Session, Some("p"), 40, 2),
            node("c:x", NodeKind::Agent, Some("c"), 44, 8),
            node("o", NodeKind::Session, None, 2, 16),
            node("z", NodeKind::Session, None, 130, 2),
        ];
        let groups = vec![
            Group { id: "g".into(), name: "Team".into(), x: 0, y: 0, w: 110, h: 30, ..Default::default() },
            Group { id: "g-in".into(), name: "Inner".into(), x: 38, y: 1, w: 34, h: 13, ..Default::default() },
        ];
        let mut doc = Doc { nodes, groups, ..Doc::default() };
        doc.view.x = 0;
        doc.view.y = 0;
        doc
    }

    /// A window on `doc` whose edits go to a service (key "k", client 7):
    /// what it sends arrives on the receiver.
    pub fn app_on(doc: Doc, gpu: bool) -> (App, Receiver<ClientMsg>) {
        let (tx, sent) = bounded(1024);
        let (_service, service_rx) = bounded(1);
        let (app_tx, _app_rx) = unbounded();
        let opened = Opened { key: "k".into(), version: 1, path: String::new(), client: 7, doc };
        let mut app = App::new(Conn::detached(tx, service_rx), Arc::new(crate::rpc::Offline), opened, true, ".".into(), app_tx, None);
        app.layout.canvas = ratatui::layout::Rect::new(0, 0, 200, 60);
        app.selection.clear();
        app.notice = None;
        if gpu {
            app.render_mode = crate::view::RenderMode::Gpu;
            app.view_scale = 1.0;
        }
        (app, sent)
    }

    pub fn app(gpu: bool) -> (App, Receiver<ClientMsg>) {
        app_on(doc(), gpu)
    }

    pub fn mouse(app: &mut App, kind: MouseEventKind, (x, y): (f32, f32), mods: Modifiers) {
        app.on_input(vec![InputEvent::Mouse(MouseEvent::at(kind, x, y, mods))]);
    }

    /// Press, four moves and release.
    pub fn drag(app: &mut App, button: MouseButton, from: (f32, f32), to: (f32, f32), mods: Modifiers) {
        mouse(app, MouseEventKind::Down(button), from, mods);
        for i in 1..=4 {
            let t = i as f32 / 4.0;
            mouse(app, MouseEventKind::Drag(button), (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t), mods);
        }
        mouse(app, MouseEventKind::Up(button), to, mods);
    }

    pub fn click(app: &mut App, button: MouseButton, at: (f32, f32), mods: Modifiers) {
        mouse(app, MouseEventKind::Down(button), at, mods);
        mouse(app, MouseEventKind::Up(button), at, mods);
    }

    pub fn key(app: &mut App, code: KeyCode, mods: Modifiers) {
        app.on_input(vec![InputEvent::Key(KeyEvent::new(code, mods))]);
    }

    pub fn ctrl(app: &mut App, c: char) {
        key(app, KeyCode::Char(c), Modifiers::CONTROL);
    }

    pub fn at(app: &App, id: &str) -> (i32, i32) {
        app.doc.node(id).map(|n| (n.x, n.y)).unwrap_or((i32::MIN, i32::MIN))
    }

    pub fn frame(app: &App, id: &str) -> (i32, i32, i32, i32) {
        app.doc.group(id).map(|g| (g.x, g.y, g.w, g.h)).unwrap_or_default()
    }

    /// Every box's place and every group's frame.
    pub type Layout = (Vec<(String, i32, i32)>, Vec<(String, i32, i32, i32, i32)>);

    pub fn layout(app: &App) -> Layout {
        (app.doc.nodes.iter().map(|n| (n.id.clone(), n.x, n.y)).collect(), app.doc.groups.iter().map(|g| (g.id.clone(), g.x, g.y, g.w, g.h)).collect())
    }

    /// Launches and stops the window asked for.
    pub fn starts_or_stops(sent: &Receiver<ClientMsg>) -> usize {
        sent.try_iter().filter(|m| matches!(m, ClientMsg::Launch { .. } | ClientMsg::Stop { .. })).count()
    }

    pub fn notice(app: &App) -> String {
        app.notice.as_ref().map(|n| n.text.clone()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;
    use crate::app::Focus;
    use crate::doc::NodeKind;
    use crate::input::{KeyCode, Modifiers, MouseButton};
    use crate::protocol::{ClientMsg, Input, Key, MOD_CTRL, ServiceMsg, TermInfo};
    use crate::sync::Op;

    const NONE: Modifiers = Modifiers::NONE;

    #[test]
    fn a_box_drag_is_one_step_and_ctrl_z_puts_it_and_its_subagent_back() {
        for gpu in [false, true] {
            let (mut app, sent) = app(gpu);
            let before = layout(&app);
            drag(&mut app, MouseButton::Left, (5.5, 3.5), (10.5, 6.5), NONE);
            assert_eq!((at(&app, "p"), at(&app, "p:helper")), ((7, 5), (11, 11)), "the box and its subagent box, gpu {gpu}");
            assert_eq!(at(&app, "c"), (40, 2), "a subsession keeps its place");
            assert_eq!(app.undo.len(), 1, "one drag, one step (gpu {gpu})");
            assert!(!app.undo.recording());
            assert_eq!(notice(&app), "Moved 2 boxes · Ctrl+Z undo move");
            assert_eq!(app.undo_label(), "Undo move");
            ctrl(&mut app, 'z');
            assert_eq!(layout(&app), before, "exactly where they were (gpu {gpu})");
            assert!(notice(&app).starts_with("Undid: moved 2 boxes"), "{}", notice(&app));
            // Redo and undo again: the same two places.
            ctrl(&mut app, 'y');
            assert_eq!((at(&app, "p"), at(&app, "p:helper")), ((7, 5), (11, 11)));
            key(&mut app, KeyCode::Char('Z'), Modifiers::CONTROL | Modifiers::SHIFT);
            assert_eq!(notice(&app), "Nothing to redo.", "Ctrl+Shift+Z redoes too (nothing left)");
            ctrl(&mut app, 'z');
            assert_eq!(layout(&app), before);
            assert_eq!(starts_or_stops(&sent), 0, "undo never starts or stops a session");
        }
    }

    #[test]
    fn a_group_drag_and_its_resize_undo_one_at_a_time_with_everything_inside() {
        for gpu in [false, true] {
            let (mut app, _sent) = app(gpu);
            let before = layout(&app);
            // The title row moves the group with its boxes, their subagents and the nested group.
            drag(&mut app, MouseButton::Left, (10.5, 0.5), (14.5, 2.5), NONE);
            for id in ["p", "p:helper", "c", "c:x", "o"] {
                let old = before.0.iter().find(|n| n.0 == id).map(|n| (n.1, n.2)).unwrap();
                assert_eq!(at(&app, id), (old.0 + 4, old.1 + 2), "{id} moved with the group (gpu {gpu})");
            }
            assert_eq!((frame(&app, "g"), frame(&app, "g-in")), ((4, 2, 110, 30), (42, 3, 34, 13)));
            assert_eq!(at(&app, "z"), (130, 2), "outside the group");
            assert!(notice(&app).starts_with("Moved group “Team” (5 boxes)"), "{}", notice(&app));
            let moved = layout(&app);
            // ◢ at the bottom-right corner: a second step.
            drag(&mut app, MouseButton::Left, (113.5, 31.5), (119.5, 34.5), NONE);
            assert_eq!(frame(&app, "g"), (4, 2, 116, 33));
            assert_eq!((app.undo.len(), app.undo_label()), (2, "Undo resize"));
            ctrl(&mut app, 'z');
            assert_eq!(layout(&app), moved, "the resize first");
            ctrl(&mut app, 'z');
            assert_eq!(layout(&app), before, "then the move, nested group and subagents included (gpu {gpu})");
            ctrl(&mut app, 'z');
            assert_eq!(notice(&app), "Nothing to undo.");
            ctrl(&mut app, 'y');
            ctrl(&mut app, 'y');
            assert_eq!(frame(&app, "g"), (4, 2, 116, 33), "both redone");
        }
    }

    #[test]
    fn a_mixed_selection_moves_each_box_once() {
        let (mut app, _sent) = app(false);
        // The parent, its subagent box and its subsession: the child must not move twice.
        app.selection = vec!["p".into(), "p:helper".into(), "c".into()];
        drag(&mut app, MouseButton::Left, (5.5, 3.5), (8.5, 4.5), NONE);
        assert_eq!(at(&app, "p"), (5, 3));
        assert_eq!(at(&app, "p:helper"), (9, 9), "selected and following its selected parent: moved once");
        assert_eq!(at(&app, "c"), (43, 3));
        assert_eq!(at(&app, "c:x"), (47, 9), "the selected subsession's subagent follows it once");
        assert_eq!(at(&app, "o"), (2, 16));
        assert_eq!(app.doc.nodes_moving_with(&app.selection.clone()).len(), 4, "each box once");
        ctrl(&mut app, 'z');
        assert_eq!(layout(&app), layout(&super::fixture::app(false).0));
        // A selected group moves its members once as well (arrow keys).
        app.select_group("g");
        key(&mut app, KeyCode::Right, NONE);
        assert_eq!((at(&app, "p"), at(&app, "c:x"), frame(&app, "g-in")), ((4, 2), (46, 8), (40, 1, 34, 13)));
    }

    #[test]
    fn a_burst_of_arrow_keys_is_one_step() {
        let (mut app, _sent) = app(false);
        app.select_node("p", false);
        for code in [KeyCode::Right, KeyCode::Right, KeyCode::Down, KeyCode::Right] {
            key(&mut app, code, NONE);
        }
        assert!(app.undo.recording() && app.undo.is_empty(), "still one burst");
        assert_eq!(at(&app, "p"), (8, 3));
        // Another target starts another burst; a click ends one.
        app.select_node("o", false);
        key(&mut app, KeyCode::Up, NONE);
        key(&mut app, KeyCode::Up, Modifiers::SHIFT);
        assert_eq!(at(&app, "o"), (2, 10));
        assert_eq!(app.undo.len(), 1);
        // A second without keys ends it too.
        std::thread::sleep(BURST + Duration::from_millis(50));
        app.on_tick();
        assert_eq!((app.undo.len(), app.undo.recording()), (2, false));
        assert_eq!(notice(&app), "Moved “o” · Ctrl+Z undo move");
        ctrl(&mut app, 'z');
        assert_eq!((at(&app, "o"), at(&app, "p")), ((2, 16), (8, 3)));
        ctrl(&mut app, 'z');
        assert_eq!((at(&app, "p"), at(&app, "p:helper")), ((2, 2), (6, 8)), "the whole burst, its subagent too");
    }

    #[test]
    fn removals_syncs_and_moves_share_one_history_newest_first() {
        let (mut app, _sent) = app(false);
        drag(&mut app, MouseButton::Left, (5.5, 3.5), (8.5, 3.5), NONE);
        app.run(crate::dialogs::Action::RemoveNode("o".into()));
        drag(&mut app, MouseButton::Left, (45.5, 3.5), (45.5, 5.5), NONE);
        assert_eq!(app.undo.len(), 3);
        ctrl(&mut app, 'z');
        assert_eq!(at(&app, "c"), (40, 2), "the last move first");
        assert_eq!(app.undo_label(), "Undo remove");
        ctrl(&mut app, 'z');
        assert_eq!(at(&app, "o"), (2, 16), "then the removal");
        assert!(!app.undo.can_redo(), "a removal is not redone; nothing before it is either");
        ctrl(&mut app, 'z');
        assert_eq!(at(&app, "p"), (2, 2), "then the first move");
        assert!(app.undo.can_redo());
        // A VS Code sync's snapshot is a step in the same history.
        let synced = app.doc.clone();
        app.record_undo(Undo::Doc("VS Code sync".into(), Box::new(synced)));
        assert!(!app.undo.can_redo(), "a new change clears what was undone");
        assert_eq!(app.undo_label(), "Undo last sync");
        // The history keeps the newest LIMIT steps.
        for i in 0..LIMIT as i32 + 5 {
            app.move_nodes_recorded(&["z".into()], 1, 0, Source::Mouse);
            app.commit_layout();
            assert_eq!(at(&app, "z").0, 131 + i);
        }
        assert_eq!(app.undo.len(), LIMIT);
    }

    #[test]
    fn undo_restores_what_is_still_there_and_never_brings_back_removed_boxes() {
        let (mut app, sent) = app(false);
        drag(&mut app, MouseButton::Left, (10.5, 0.5), (12.5, 1.5), NONE);
        // The service applies the move (this window's own patch) …
        app.flush_doc();
        let ops = sent.try_iter().find_map(|m| if let ClientMsg::DocEdit { ops_json, .. } = m { Some(ops_json) } else { None }).expect("the move went out");
        app.on_service(ServiceMsg::DocPatch { key: "k".into(), version: 2, origin: 7, ops_json: ops });
        // … then another window removes "o" and moves "p".
        let mut moved = app.doc.node("p").unwrap().clone();
        moved.x = 60;
        let theirs = serde_json::to_string(&[Op::DelNode("o".into()), Op::NodeFields(Box::new(moved), vec!["x".into()])]).unwrap();
        app.on_service(ServiceMsg::DocPatch { key: "k".into(), version: 3, origin: 8, ops_json: theirs });
        assert!(app.doc.node("o").is_none() && at(&app, "p") == (60, 3));
        ctrl(&mut app, 'z');
        assert!(app.doc.node("o").is_none(), "a layout undo never brings a removed box back");
        assert_eq!(at(&app, "p"), (60, 3), "moved elsewhere since: left where the other window put it");
        assert_eq!((at(&app, "p:helper"), at(&app, "c"), frame(&app, "g"), frame(&app, "g-in")), ((6, 8), (40, 2), (0, 0, 110, 30), (38, 1, 34, 13)));
        assert_eq!(notice(&app), "Undid: moved group “Team” (5 boxes), 5 items restored; 1 item removed since, 1 item moved elsewhere since (left as they are).");
        // The undo is an ordinary edit: positions go to the service like any move.
        app.flush_doc();
        let edit = sent.try_iter().find_map(|m| if let ClientMsg::DocEdit { ops_json, .. } = m { Some(ops_json) } else { None }).expect("the undo went out");
        let ops: Vec<Op> = serde_json::from_str(&edit).unwrap();
        assert!(ops.iter().any(|op| matches!(op, Op::NodeFields(n, _) if n.id == "c")) && ops.iter().any(|op| matches!(op, Op::GroupFields(g, _) if g.id == "g")), "{ops:?}");
        assert!(!ops.iter().any(|op| matches!(op, Op::PutNode(_))), "nothing created: {ops:?}");
    }

    #[test]
    fn ctrl_z_belongs_to_the_terminal_and_text_fields_that_have_the_keyboard() {
        let (mut app, sent) = app(false);
        drag(&mut app, MouseButton::Left, (5.5, 3.5), (8.5, 3.5), NONE);
        let moved = layout(&app);
        // A terminal with the keyboard: the program gets Ctrl+Z (agents and shells undo with it).
        app.terms.insert("p".into(), TermInfo { id: "p".into(), alive: true, ..Default::default() });
        app.focused_term = Some("p".into());
        app.focus = Focus::Terminal;
        let _ = sent.try_iter().count();
        ctrl(&mut app, 'z');
        let keys: Vec<Input> = sent.try_iter().filter_map(|m| if let ClientMsg::Input { input, .. } = m { Some(input) } else { None }).collect();
        assert_eq!(keys, [Input::Key { key: Key::Char('z'), mods: MOD_CTRL }]);
        assert_eq!((layout(&app), app.undo.len()), (moved.clone(), 1), "the canvas kept its move");
        // The sidebar filter and a dialog keep Ctrl+Z to themselves.
        app.focus = Focus::Canvas;
        app.run(crate::dialogs::Action::Rename(crate::dialogs::Target::Node("p".into())));
        ctrl(&mut app, 'z');
        assert_eq!(layout(&app), moved, "a dialog's text field");
        app.modals.clear();
        key(&mut app, KeyCode::Char('/'), NONE);
        ctrl(&mut app, 'z');
        assert_eq!((layout(&app), app.side_filter_active), (moved.clone(), true), "the sidebar filter");
        key(&mut app, KeyCode::Esc, NONE);
        // On the canvas it is the canvas's undo.
        app.set_focus(Focus::Canvas);
        ctrl(&mut app, 'z');
        assert_eq!(at(&app, "p"), (2, 2));
    }

    #[test]
    fn fit_to_members_is_a_step_and_stacking_is_not() {
        let (mut app, _sent) = app(false);
        app.run(crate::dialogs::Action::FitGroup("g-in".into()));
        assert_ne!(frame(&app, "g-in"), (38, 1, 34, 13));
        assert_eq!(app.undo_label(), "Undo fit");
        ctrl(&mut app, 'z');
        assert_eq!(frame(&app, "g-in"), (38, 1, 34, 13));
        // Stacking moves nobody (the stack keeps its boxes' places): no step.
        app.set_stacked("p", true);
        assert!(app.undo.is_empty() && app.doc.node("p").is_some_and(|n| n.stacked));
        // Dragging the stacked parent carries its stack; one undo brings all back.
        drag(&mut app, MouseButton::Left, (5.5, 3.5), (9.5, 3.5), NONE);
        assert_eq!((at(&app, "c"), at(&app, "c:x")), ((44, 2), (48, 8)));
        ctrl(&mut app, 'z');
        assert_eq!((at(&app, "p"), at(&app, "c"), at(&app, "c:x")), ((2, 2), (40, 2), (44, 8)));
        assert_eq!(app.doc.nodes.iter().filter(|n| n.kind == NodeKind::Session).count(), 4);
    }
}
