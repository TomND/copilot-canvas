//! Viewing versus starting a box. A click (canvas or sidebar), Tab and the
//! arrows only select and show: a running terminal comes into view (and
//! gets the keyboard), anything else shows its details panel. Only an
//! explicit action starts or resumes a session: a double-click, Enter, the
//! details panel's Start / Resume button or a menu item.

use crate::agents::{AgentKind, agent_of, native_id};
use crate::app::{App, Focus};
use crate::doc::{Node, NodeKind};
use crate::side::RightDetail;

/// What starting a box would do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartPlan {
    /// Its terminal runs here: show it and give it the keyboard.
    Focus,
    /// A new conversation, or the box's command.
    Start,
    /// Continue its conversation.
    Resume,
    /// It cannot start here; the reason.
    Blocked(String),
}

impl StartPlan {
    /// The button or menu label (`None` when it cannot start).
    pub fn label(&self) -> Option<&'static str> {
        match self {
            StartPlan::Focus => Some("Open terminal"),
            StartPlan::Start => Some("Start"),
            StartPlan::Resume => Some("Resume"),
            StartPlan::Blocked(_) => None,
        }
    }
}

impl App {
    /// The session box a box starts: itself, or a subagent's parent.
    fn start_target(&self, node: &Node) -> Option<Node> {
        match node.kind {
            NodeKind::Session => Some(node.clone()),
            NodeKind::Agent => node.parent.as_deref().and_then(|p| self.doc.node(p)).cloned(),
            NodeKind::Automation => None,
        }
    }

    /// What `start_node` would do with this box.
    pub fn start_plan(&self, node: &Node) -> StartPlan {
        let Some(session) = self.start_target(node) else {
            return StartPlan::Blocked(if node.kind == NodeKind::Automation { "Automations run from their details panel (Run now).".into() } else { "Its parent session is not on the canvas.".into() });
        };
        if self.owned_alive(&session.id) || self.launching.contains_key(&session.id) {
            return StartPlan::Focus;
        }
        if session.passive.is_some() {
            return StartPlan::Blocked("A passive session runs from its panel (Run now, Open chat).".into());
        }
        if self.preparing.contains(&session.id) {
            return StartPlan::Blocked("Its worktree is still being created.".into());
        }
        if self.summary(&session).is_some_and(|s| s.live) {
            return StartPlan::Blocked("It is open in another terminal (CLI or VS Code); Copilot Canvas never starts a second copy.".into());
        }
        if session.command.is_some() || session.fresh {
            return StartPlan::Start;
        }
        let kind = agent_of(&session);
        // An agent that names its session on the first prompt has nothing to resume before it.
        if kind != AgentKind::Copilot && !kind.is_session_id(native_id(&session)) {
            return StartPlan::Start;
        }
        StartPlan::Resume
    }

    /// A click or a sidebar row: selects and shows the box, never launches.
    /// A running terminal comes into view with the keyboard; a box that is
    /// not running shows its details panel (passive sessions and automations
    /// keep their own panels).
    pub fn view_node(&mut self, id: &str) {
        let Some(node) = self.doc.node(id).cloned() else { return };
        self.select_node(id, false);
        match node.kind {
            NodeKind::Automation => {}
            NodeKind::Session if node.passive.is_some() => {}
            NodeKind::Session if self.owned_alive(id) || self.launching.contains_key(id) => {
                self.show_terminal(id);
                self.set_focus(Focus::Terminal);
            }
            NodeKind::Session | NodeKind::Agent => self.right_detail = Some(RightDetail::Session(id.to_owned())),
        }
        self.needs_draw = true;
    }

    /// The explicit start: double-click, Enter, Start / Resume. Launches the
    /// box's session when it is not running here (a subagent's parent) and
    /// gives its terminal the keyboard. Never starts a second copy of a
    /// session that is open elsewhere.
    pub fn start_node(&mut self, id: &str) {
        let Some(node) = self.doc.node(id).cloned() else { return };
        if node.kind == NodeKind::Automation {
            return self.view_node(id);
        }
        // A subagent's own conversation (an OpenCode child) resumes in a
        // terminal of its own when nothing else writes to it.
        if node.kind == NodeKind::Agent && matches!(self.child_resume(&node), crate::inspect::ChildResume::Allowed { .. } | crate::inspect::ChildResume::Existing(_)) {
            return self.resume_child(id);
        }
        self.select_node(id, false);
        let Some(session) = self.start_target(&node) else { return self.notify("Its parent session is not on the canvas.", true) };
        let session = session.id;
        if self.owned_alive(&session) {
            self.show_terminal(&session);
            self.set_focus(Focus::Terminal);
            return;
        }
        match self.start_plan(&node) {
            StartPlan::Blocked(_) if self.doc.node(&session).is_some_and(|n| n.passive.is_some()) => {
                // A passive session opens its panel; its chat starts only on request.
                self.right_detail = Some(RightDetail::Passive(session));
                self.needs_draw = true;
            }
            StartPlan::Blocked(reason) => self.notify(reason, true),
            StartPlan::Focus | StartPlan::Start | StartPlan::Resume => self.launch(&session),
        }
    }

    /// Selection cleared (a click on empty canvas, Esc); a session's details
    /// panel goes with it.
    pub fn clear_selection(&mut self) {
        self.selection.clear();
        self.selected_group = None;
        if matches!(self.right_detail, Some(RightDetail::Session(_))) {
            self.right_detail = None;
        }
        self.needs_draw = true;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossbeam_channel::{Receiver, bounded, unbounded};

    use super::*;
    use crate::conn::Conn;
    use crate::doc::Doc;
    use crate::protocol::{ClientMsg, SessionSummary, TermInfo};
    use crate::sync::Opened;

    fn node(id: &str, command: bool) -> Node {
        let mut node: Node = serde_json::from_value(serde_json::json!({ "id": id, "kind": "session", "name": id, "x": 0, "y": 0, "reviewed_ms": 1, "cwd": "." })).unwrap();
        if command {
            node.command = Some(vec!["powershell.exe".into()]);
            node.fresh = true;
        }
        node
    }

    fn app(nodes: Vec<Node>) -> (App, Receiver<ClientMsg>) {
        let (tx, sent) = bounded(256);
        let (_service, service_rx) = bounded(1);
        let (app_tx, _app_rx) = unbounded();
        let doc = Doc { nodes, ..Doc::default() };
        let app = App::new(Conn::detached(tx, service_rx), Arc::new(crate::rpc::Offline), Opened::detached(doc), true, ".".into(), app_tx, None);
        (app, sent)
    }

    fn launches(sent: &Receiver<ClientMsg>) -> Vec<String> {
        sent.try_iter().filter_map(|m| if let ClientMsg::Launch { id, .. } = m { Some(id) } else { None }).collect()
    }

    #[test]
    fn viewing_never_launches_and_starting_does() {
        let (mut app, sent) = app(vec![node("a", true), node("b", true)]);
        app.view_node("a");
        assert!(launches(&sent).is_empty(), "a click only shows");
        assert_eq!(app.selection, ["a"]);
        assert_eq!(app.right_detail, Some(RightDetail::Session("a".into())), "its details instead of a terminal");
        assert_eq!(app.start_plan(app.doc.node("a").unwrap()), StartPlan::Start);
        app.start_node("a");
        assert_eq!(launches(&sent), ["a"], "the explicit start launches");
        assert_eq!((app.focus, app.right_detail.clone()), (Focus::Terminal, None));
        // Running: a click shows its terminal with the keyboard, nothing is launched again.
        app.terms.insert("a".into(), TermInfo { id: "a".into(), alive: true, ..Default::default() });
        app.set_focus(Focus::Canvas);
        app.view_node("b");
        app.view_node("a");
        assert!(launches(&sent).is_empty());
        assert_eq!((app.focus, app.focused_term.as_deref()), (Focus::Terminal, Some("a")));
        assert_eq!(app.start_plan(app.doc.node("a").unwrap()), StartPlan::Focus);
        app.start_node("a");
        assert!(launches(&sent).is_empty(), "starting a running session only focuses it");
    }

    #[test]
    fn sessions_open_elsewhere_and_passive_sessions_never_start_here() {
        let mut elsewhere = node("e", false);
        elsewhere.agent_session = None;
        let mut passive = node("p", false);
        passive.passive = Some(Default::default());
        let (mut app, sent) = app(vec![elsewhere, passive]);
        app.sessions.insert("e".into(), SessionSummary { id: "e".into(), live: true, tracked: true, ..Default::default() });
        assert!(matches!(app.start_plan(app.doc.node("e").unwrap()), StartPlan::Blocked(reason) if reason.contains("second copy")));
        app.start_node("e");
        app.start_node("p");
        assert!(launches(&sent).is_empty());
        assert_eq!(app.right_detail, Some(RightDetail::Passive("p".into())), "a passive session shows its panel");
        assert!(app.notice.is_some());
    }

    #[test]
    fn resume_needs_a_conversation_and_exited_terminals_give_way_to_details() {
        let mut copilot = node("12345678-1234-1234-1234-123456789abc", false);
        copilot.fresh = false;
        let mut opencode = node("t-oc", false);
        opencode.agent = Some("opencode".into());
        let (mut app, _sent) = app(vec![copilot.clone(), opencode.clone()]);
        assert_eq!(app.start_plan(&copilot), StartPlan::Resume);
        assert_eq!(app.start_plan(&opencode), StartPlan::Start, "no ses_ id yet: nothing to resume");
        opencode.agent_session = Some("ses_abc".into());
        assert_eq!(app.start_plan(&opencode), StartPlan::Resume);
        // An exited terminal stays in view only while you are in it.
        let id = copilot.id.clone();
        app.terms.insert(id.clone(), TermInfo { id: id.clone(), alive: false, exit_code: Some(0), ..Default::default() });
        app.select_node(&id, false);
        app.focused_term = Some(id.clone());
        app.focus = Focus::Terminal;
        assert_eq!(app.visible_terminals(), std::slice::from_ref(&id));
        app.focus = Focus::Canvas;
        assert!(app.visible_terminals().is_empty(), "the details panel shows instead");
        app.view_node(&id);
        assert_eq!(app.right_detail, Some(RightDetail::Session(id)));
        app.clear_selection();
        assert!(app.right_detail.is_none() && app.selection.is_empty());
    }

    fn mouse(app: &mut App, kind: crate::input::MouseEventKind, x: f32, y: f32, mods: crate::input::Modifiers) {
        app.on_mouse(crate::input::MouseEvent::at(kind, x, y, mods));
    }

    fn click(app: &mut App, x: f32, y: f32) {
        use crate::input::{Modifiers, MouseButton, MouseEventKind};
        mouse(app, MouseEventKind::Down(MouseButton::Left), x, y, Modifiers::NONE);
        mouse(app, MouseEventKind::Up(MouseButton::Left), x, y, Modifiers::NONE);
    }

    /// The demo canvas (group g-demo at world 0,0 70 x 15; boxes inside) with
    /// a nested group, on a 100 x 40 canvas at the origin.
    fn groups_app(gpu: bool) -> (App, Receiver<ClientMsg>) {
        let mut doc = crate::demo::demo_doc(".");
        doc.groups.push(crate::doc::Group { id: "g-inner".into(), name: "Inner".into(), x: 2, y: 10, w: 30, h: 4, ..Default::default() });
        doc.view.x = 0;
        doc.view.y = 0;
        let (tx, sent) = bounded(256);
        let (_service, service_rx) = bounded(1);
        let (app_tx, _app_rx) = unbounded();
        let mut app = App::new(Conn::detached(tx, service_rx), Arc::new(crate::rpc::Offline), Opened::detached(doc), true, ".".into(), app_tx, None);
        app.layout.canvas = ratatui::layout::Rect::new(0, 0, 100, 40);
        if gpu {
            app.render_mode = crate::view::RenderMode::Gpu;
            app.view_scale = 1.0;
        }
        app.selection.clear();
        (app, sent)
    }

    #[test]
    fn a_click_on_a_groups_background_or_border_selects_the_innermost_group() {
        use crate::app::CanvasHit;
        for gpu in [false, true] {
            let (mut app, sent) = groups_app(gpu);
            // Empty interior of the outer group (no box or nested group at world 34, 13).
            click(&mut app, 34.5, 13.5);
            assert_eq!(app.selected_group.as_deref(), Some("g-demo"), "gpu {gpu}");
            // The nested group's interior and its left border: the inner one.
            app.clear_selection();
            click(&mut app, 15.5, 12.5);
            assert_eq!(app.selected_group.as_deref(), Some("g-inner"), "gpu {gpu}");
            app.clear_selection();
            click(&mut app, 2.5, 11.5);
            assert_eq!(app.selected_group.as_deref(), Some("g-inner"), "the inner group's border, gpu {gpu}");
            // The outer group's bottom border row and left border column.
            app.clear_selection();
            click(&mut app, 40.5, 14.5);
            assert_eq!(app.selected_group.as_deref(), Some("g-demo"), "bottom border, gpu {gpu}");
            app.clear_selection();
            click(&mut app, 0.5, 9.5);
            assert_eq!(app.selected_group.as_deref(), Some("g-demo"), "left border, gpu {gpu}");
            // Outside every group: the selection clears.
            click(&mut app, 90.5, 30.5);
            assert!(app.selected_group.is_none() && app.selection.is_empty());
            assert!(!sent.try_iter().any(|m| matches!(m, ClientMsg::Launch { .. })), "nothing launches");
            // Boxes, titles and the resize corner keep their meaning.
            assert!(matches!(app.canvas_hit(5, 4), CanvasHit::Node(id) if id == "demo-main"));
            assert!(matches!(app.canvas_hit(10, 0), CanvasHit::GroupTitle(id) if id == "g-demo"));
            assert!(matches!(app.canvas_hit(69, 14), CanvasHit::GroupResize(id) if id == "g-demo"));
        }
        // The GUI's border is hit a few pixels outside the outline too, not further.
        let (app, _sent) = groups_app(true);
        assert!(matches!(app.canvas_hit_f((70.3, 9.5)), CanvasHit::GroupBody(id) if id == "g-demo"));
        assert!(matches!(app.canvas_hit_f((5.5, 15.2)), CanvasHit::GroupBody(id) if id == "g-demo"));
        assert!(matches!(app.canvas_hit_f((71.0, 9.5)), CanvasHit::Empty));
    }

    #[test]
    fn group_background_gestures_still_pan_and_double_click_creates_a_session() {
        use crate::input::{Modifiers, MouseButton, MouseEventKind};
        let (mut app, _sent) = groups_app(false);
        // A drag pans (and selects nothing).
        let view = (app.doc.view.x, app.doc.view.y);
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 20.5, 12.5, Modifiers::NONE);
        mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 25.5, 13.5, Modifiers::NONE);
        mouse(&mut app, MouseEventKind::Up(MouseButton::Left), 25.5, 13.5, Modifiers::NONE);
        assert_ne!((app.doc.view.x, app.doc.view.y), view, "panned");
        assert!(app.selected_group.is_none());
        // Shift-drag is a rubber band.
        app.last_click = None;
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 20.5, 12.5, Modifiers::SHIFT);
        assert!(matches!(app.drag, Some(crate::app::Drag::Rubber { .. })));
        mouse(&mut app, MouseEventKind::Up(MouseButton::Left), 20.5, 12.5, Modifiers::SHIFT);
        // A double-click on the background asks for a new session there.
        app.last_click = None;
        click(&mut app, 20.5, 12.5);
        click(&mut app, 20.5, 12.5);
        assert!(matches!(app.modals.last(), Some(crate::dialogs::Modal::NewSession(_))));
    }
}
