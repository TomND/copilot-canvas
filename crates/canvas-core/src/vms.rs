//! VM sessions in the client (docs/VM-SESSIONS.md): the marker of a box that
//! runs on a VM, and what shows only when the service has a VM pack (the
//! bus snapshot's `vm` view). Without a pack nothing here is drawn.

/// The mark of VM sessions (one cell wide in every console: see the test).
pub const VM_GLYPH: &str = "☁";
/// Before a VM session's name on its box.
pub const VM_PREFIX: &str = "☁ ";

/// A box's VM as the service last told (`None`: not a VM box, or no pack).
pub fn box_vm<'a>(app: &'a crate::app::App, id: &str) -> Option<&'a canvas_model::vm::VmInfo> {
    app.bus.vm.as_ref()?.boxes.get(id)
}

/// What a VM box's status line says while its VM matters more than its
/// agent's state (`None` while it simply runs): the text and its colour.
pub fn state_line(info: &canvas_model::vm::VmInfo, now_ms: u64) -> Option<(String, ratatui::style::Color)> {
    use crate::paint::{ACCENT, ATTENTION, MUTED, NEGATIVE};
    use canvas_model::vm::VmState;
    let note = |text: &str| if info.note.is_empty() { text.to_owned() } else { format!("{text}: {}", info.note) };
    Some(match info.state {
        VmState::Running if info.offline_ms == 0 => return None,
        VmState::Running | VmState::IntegrationOffline => (format!("{VM_GLYPH} integration offline{}", if info.offline_ms > 0 { format!(" {}", crate::paths::format_age(info.offline_ms, now_ms)) } else { String::new() }), ATTENTION),
        VmState::Queued => (format!("{VM_GLYPH} {}", note("queued")), MUTED),
        VmState::Claiming => (format!("{VM_GLYPH} claiming a VM…"), ACCENT),
        VmState::Starting => (format!("{VM_GLYPH} starting…"), ACCENT),
        VmState::Suspending => (format!("{VM_GLYPH} suspending…"), MUTED),
        VmState::Suspended => (format!("{VM_GLYPH} suspended"), MUTED),
        VmState::Resuming => (format!("{VM_GLYPH} resuming…"), ACCENT),
        VmState::Lost => (format!("{VM_GLYPH} VM lost"), NEGATIVE),
        VmState::Deleting => (format!("{VM_GLYPH} deleting its VM…"), MUTED),
        VmState::Deleted => (format!("{VM_GLYPH} VM deleted"), MUTED),
        VmState::Failed => (format!("{VM_GLYPH} {}", note("could not start")), NEGATIVE),
    })
}

impl crate::app::App {
    /// Asks the service for an operation on box `id`'s VM (off the UI
    /// thread: a delete checks the VM's repositories first); its answer is a
    /// notice, or for a delete the barrier stopped, what would be lost and
    /// *Delete anyway*.
    pub fn vm_action(&mut self, id: String, op: canvas_model::vm::VmOp) {
        let deleting = matches!(op, canvas_model::vm::VmOp::Delete { .. });
        let call_id = id.clone();
        self.in_background(
            move |backend| backend.vm_action(&call_id, op).map_err(|e| format!("{e:#}")),
            move |result| crate::app::AppEvent::Run(Box::new(move |app: &mut crate::app::App| app.vm_answered(&id, deleting, result))),
        );
    }

    fn vm_answered(&mut self, id: &str, deleting: bool, result: Result<String, String>) {
        let findings = result.as_ref().err().filter(|_| deleting).and_then(|e| canvas_model::vm::refused_findings(e));
        match (result, findings) {
            (Ok(text), _) => self.notify(text, false),
            (Err(_), Some(findings)) => {
                let name = self.doc.node(id).map(|n| n.name.clone()).unwrap_or_default();
                let mut body = vec!["Deleting its VM loses what its repositories have not pushed:".to_owned()];
                body.extend(findings.iter().map(|f| format!("  · {f}")));
                body.push("Push what you keep first (ask its agent), or delete anyway.".into());
                self.modals.push(crate::dialogs::Modal::Confirm(crate::dialogs::Confirm {
                    title: format!("Delete the VM of “{name}” anyway?"),
                    body,
                    buttons: vec![("Delete anyway".into(), crate::dialogs::Action::Vm { id: id.to_owned(), op: canvas_model::vm::VmOp::Delete { confirm: findings } }), ("Keep it".into(), crate::dialogs::Action::Nothing)],
                    sel: 1,
                }));
            }
            (Err(error), None) => self.notify(error, true),
        }
        self.needs_draw = true;
    }

    /// A VM box's menu items: its VM's details, and Suspend or Resume.
    pub fn vm_menu_items(&self, node: &crate::doc::Node) -> Vec<crate::dialogs::MenuItem> {
        use canvas_model::vm::{VmOp, VmState};
        let Some(info) = box_vm(self, &node.id).filter(|_| node.placement.as_ref().is_some_and(|p| p.is_vm())) else { return Vec::new() };
        let item = |label: &str, action: crate::dialogs::Action, enabled: bool| crate::dialogs::MenuItem { label: label.into(), hint: String::new(), action, enabled };
        let mut items = vec![item(&format!("{VM_GLYPH} VM details…"), crate::dialogs::Action::VmDetails(node.id.clone()), true)];
        match info.state {
            VmState::Running => items.push(item("Suspend VM", crate::dialogs::Action::Vm { id: node.id.clone(), op: VmOp::Suspend }, info.offline_ms == 0)),
            VmState::Suspended | VmState::Suspending => items.push(item("Resume VM", crate::dialogs::Action::Vm { id: node.id.clone(), op: VmOp::Resume }, true)),
            _ => {}
        }
        items
    }

    /// *Delete VM…*: one confirmation; the service's barrier may then ask again.
    pub fn ask_vm_delete(&mut self, id: &str) {
        let name = self.doc.node(id).map(|n| n.name.clone()).unwrap_or_default();
        self.modals.push(crate::dialogs::Modal::Confirm(crate::dialogs::Confirm {
            title: format!("Delete the VM of “{name}”?"),
            body: vec![
                "Its sandbox and its satellite's credential go; the box and its conversation stay.".into(),
                "What its repositories have not pushed is checked first.".into(),
            ],
            buttons: vec![("Delete VM".into(), crate::dialogs::Action::Vm { id: id.to_owned(), op: canvas_model::vm::VmOp::Delete { confirm: Vec::new() } }), ("Cancel".into(), crate::dialogs::Action::Nothing)],
            sel: 1,
        }));
        self.needs_draw = true;
    }
}

/// The agents VM sessions run so far (Copilot CLI; docs/VM-SESSIONS-PLAN.md
/// Phase 5 adds the others).
pub fn runs_on_vm(kind: crate::agents::AgentKind) -> bool {
    kind == crate::agents::AgentKind::Copilot
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialogs::{Action, Modal};
    use canvas_model::rpc::{Request, Response};
    use canvas_model::vm::VmOp;
    use std::sync::{Arc, Mutex};

    /// A service whose barrier finds an unpushed branch until it is confirmed.
    #[derive(Default)]
    struct Service {
        asked: Mutex<Vec<VmOp>>,
    }

    impl canvas_model::rpc::Backend for Service {
        fn call(&self, request: Request) -> anyhow::Result<Response> {
            let Request::VmAction { op, .. } = request else { return Ok(Response::Error("not here".into())) };
            self.asked.lock().unwrap().push(op.clone());
            Ok(match op {
                VmOp::Delete { confirm } if confirm.is_empty() => Response::Error(canvas_model::vm::barrier_refusal(&["1 branch not pushed: atelier/x".into()], "now")),
                _ => Response::Text("Deleting its VM.".into()),
            })
        }
    }

    #[test]
    fn deleting_a_vm_asks_then_shows_what_the_barrier_found_and_confirms_exactly_that() {
        let (tx, _sent) = crossbeam_channel::bounded(64);
        let (_keep, service_rx) = crossbeam_channel::bounded(1);
        let (app_tx, events) = crossbeam_channel::unbounded();
        let service = Arc::new(Service::default());
        let mut doc = crate::demo::demo_doc(".");
        doc.nodes[0].id = "b1".into();
        doc.nodes[0].name = "Fix it".into();
        let mut app = crate::app::App::new(crate::conn::Conn::detached(tx, service_rx), service.clone(), crate::sync::Opened::detached(doc), false, ".".into(), app_tx, None);
        let answered = |app: &mut crate::app::App| {
            let event = events.recv_timeout(std::time::Duration::from_secs(5)).expect("the service's answer");
            app.on_app_event(event);
        };
        app.run(Action::VmDelete("b1".into()));
        let Some(Modal::Confirm(ask)) = app.modals.pop() else { panic!("asked first") };
        assert_eq!((ask.title.as_str(), ask.sel), ("Delete the VM of “Fix it”?", 1), "Cancel is the default");
        assert!(service.asked.lock().unwrap().is_empty(), "nothing asked of the service yet");
        app.run(ask.buttons[0].1.clone());
        answered(&mut app);
        let Some(Modal::Confirm(found)) = app.modals.pop() else { panic!("what would be lost") };
        assert!(found.body.iter().any(|l| l.contains("1 branch not pushed: atelier/x")), "{:?}", found.body);
        assert_eq!(found.buttons[0].1, Action::Vm { id: "b1".into(), op: VmOp::Delete { confirm: vec!["1 branch not pushed: atelier/x".into()] } });
        app.run(found.buttons[0].1.clone());
        answered(&mut app);
        assert!(app.modals.is_empty());
        assert_eq!(service.asked.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_vm_box_says_its_vms_state_unless_it_simply_runs() {
        use canvas_model::vm::{VmInfo, VmState};
        let at = |state: VmState| VmInfo { state, ..Default::default() };
        assert_eq!(state_line(&at(VmState::Running), 0), None, "the agent's own state shows");
        let offline = VmInfo { offline_ms: 1_000, ..at(VmState::Running) };
        assert_eq!(state_line(&offline, 181_000).unwrap().0, "☁ integration offline 3m");
        let queued = VmInfo { note: "4 of 4 VMs are running".into(), ..at(VmState::Queued) };
        assert_eq!(state_line(&queued, 0).unwrap().0, "☁ queued: 4 of 4 VMs are running");
        assert_eq!(state_line(&at(VmState::Suspended), 0).unwrap().0, "☁ suspended");
        assert_eq!(state_line(&at(VmState::Lost), 0).unwrap().0, "☁ VM lost");
    }

    #[test]
    fn the_vm_glyph_is_one_cell_wide() {
        use unicode_width::UnicodeWidthStr;
        assert_eq!(VM_GLYPH.width(), 1);
        assert_eq!(VM_PREFIX.width(), 2);
        assert_eq!(VM_GLYPH.chars().count(), 1, "no variation selector (that would ask for an emoji, two cells)");
    }
}
