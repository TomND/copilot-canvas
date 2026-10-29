//! Tracked pull requests on the canvas (WI-014): the panel of the routed
//! canvas's pull requests (◎ Agents › Pull requests), their monitors and
//! follow-ups, and its buttons. The service owns everything: the panel shows
//! the bus snapshot's [`PrsView`] and its buttons are bus requests, so every
//! window and remote client sees and changes the same registry. Nothing here
//! starts a session except *Resume owner*, the explicit start of a blocked
//! follow-up's owner.

use crate::app::App;
use crate::dialogs::{Action, Confirm, Modal, Pick, PickPurpose};
use canvas_model::prs::{DispatchState, MonitorStatus, PrsView, TrackedPr};
use ratatui::style::Color;

use crate::paint::{ACCENT, ATTENTION, FINISHED, MUTED, NEGATIVE, POSITIVE};

/// The marker of pull requests (panel, agents panel).
pub const MARK: &str = "⇅";

#[derive(Clone, Debug, PartialEq)]
pub enum PrAction {
    /// The pull requests panel.
    Panel,
    /// A pull request's page (its URL) in the browser.
    Open(String),
    /// The owning session's box (viewed, never started).
    Owner(String),
    /// Polls now (a pull request's URL; empty: every active one).
    Check(String),
    /// Asks, then stops tracking (its URL).
    Untrack(String),
    UntrackNow(String),
    /// A follow-up (its id): dismiss it, send it again, give it to another session.
    Dismiss(String),
    Retry(String),
    Reassign(String),
}

/// A follow-up state's glyph and colour (colour-blind safe: the glyph differs too).
pub fn dispatch_look(state: DispatchState) -> (&'static str, Color) {
    match state {
        DispatchState::Intended | DispatchState::Queued => ("◌", ACCENT),
        DispatchState::Delivered => ("✓", POSITIVE),
        DispatchState::Blocked => ("⚠", ATTENTION),
        DispatchState::Failed => ("✗", NEGATIVE),
        DispatchState::Dismissed => ("·", MUTED),
    }
}

/// A pull request's glyph and colour in lists.
pub fn pr_look(pr: &TrackedPr) -> (&'static str, Color) {
    if pr.state.ended() {
        ("✓", FINISHED)
    } else if pr.needs_you() {
        ("⚠", ATTENTION)
    } else {
        match pr.monitor.status {
            MonitorStatus::Pending => ("◌", MUTED),
            _ => ("●", POSITIVE),
        }
    }
}

impl App {
    /// The routed canvas's pull requests (`None`: none tracked and no coordinator).
    pub fn prs_view(&self) -> Option<&PrsView> {
        self.bus.prs.as_ref().filter(|v| self.doc.canvas_id.as_deref() == Some(v.canvas.as_str()))
    }

    pub fn run_prs(&mut self, action: PrAction) {
        self.needs_draw = true;
        let request = |op: &str, extra: serde_json::Value| {
            let mut value = serde_json::json!({ "action": "prs", "op": op });
            if let (Some(target), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
                target.extend(extra.clone());
            }
            value
        };
        match action {
            PrAction::Panel => self.right_detail = Some(crate::side::RightDetail::Prs),
            PrAction::Open(url) => self.open_target(&url),
            PrAction::Owner(id) => {
                if self.doc.node(&id).is_some() {
                    self.reveal_node(&id);
                    self.select_node(&id, false);
                } else {
                    self.notify("The owning session is no longer on this canvas.", true);
                }
            }
            PrAction::Check(url) => {
                self.bus_action(request("check", serde_json::json!({ "url": url })));
                self.notify(if url.is_empty() { "Checking every active pull request now…" } else { "Checking it now…" }, false);
            }
            PrAction::Untrack(url) => {
                let short = self.prs_view().and_then(|v| v.prs.iter().find(|p| p.url == url)).map(|p| p.pr.short()).unwrap_or_else(|| url.clone());
                self.modals.push(Modal::Confirm(Confirm {
                    title: format!("Stop tracking {short}?"),
                    body: vec!["The canvas stops checking it; its queued follow-ups are withdrawn.".into(), "The pull request itself is not touched.".into()],
                    buttons: vec![("Stop tracking".into(), Action::Prs(PrAction::UntrackNow(url))), ("Cancel".into(), Action::Nothing)],
                    sel: 1,
                }));
            }
            PrAction::UntrackNow(url) => self.bus_action(request("untrack", serde_json::json!({ "url": url }))),
            PrAction::Dismiss(id) => self.bus_action(request("dismiss", serde_json::json!({ "dispatch": id }))),
            PrAction::Retry(id) => {
                self.bus_action(request("retry", serde_json::json!({ "dispatch": id })));
                self.notify("Sending the follow-up again (it is typed in when the session is idle).", false);
            }
            PrAction::Reassign(id) => {
                let items: Vec<(String, String)> = self
                    .doc
                    .nodes
                    .iter()
                    .filter(|n| n.kind == crate::doc::NodeKind::Session && n.command.is_none() && n.passive.is_none())
                    .map(|n| (n.name.clone(), format!("{} · {}", self.doc.group_of(n).map(|g| g.name.clone()).unwrap_or_else(|| "no group".into()), n.id)))
                    .collect();
                if items.is_empty() {
                    return self.notify("This canvas has no other session to give it to.", true);
                }
                self.modals.push(Modal::Pick(Pick { purpose: PickPurpose::PrReassign(id), title: "Give the follow-up to (it is typed in when that session is idle)".into(), items, filter: String::new(), sel: 0, scroll: 0 }));
            }
        }
    }

    /// A session picked for a follow-up (by its row's name).
    pub fn reassign_follow_up(&mut self, dispatch: &str, name: &str) {
        let Some(id) = self.doc.nodes.iter().find(|n| n.name == name && n.passive.is_none() && n.command.is_none()).map(|n| n.id.clone()) else { return };
        self.bus_action(serde_json::json!({ "action": "prs", "op": "reassign", "dispatch": dispatch, "to": id }));
        self.notify(format!("The follow-up goes to “{name}” (when it is idle)."), false);
    }

    /// What the agents panel and an agent's panel say about the pull requests.
    pub fn prs_summary(&self) -> Option<String> {
        let view = self.prs_view()?;
        let mut text = format!("{} tracked, {} active", view.prs.len(), view.active());
        if view.needing_you() > 0 {
            text.push_str(&format!(", {} need you", view.needing_you()));
        }
        if !view.paused.is_empty() {
            text.push_str(" · not monitored");
        }
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::Doc;
    use crate::protocol::ClientMsg;
    use canvas_model::prs::{Dispatch, PrRef, Registry, Role};

    #[test]
    fn the_panel_shows_the_services_view_and_its_buttons_are_service_requests() {
        let doc = Doc { canvas_id: Some("c1".into()), ..Doc::default() };
        let (tx, sent) = crossbeam_channel::bounded(256);
        let (_service, service_rx) = crossbeam_channel::bounded(1);
        let (app_tx, _app_rx) = crossbeam_channel::unbounded();
        let mut app = App::new(crate::conn::Conn::detached(tx, service_rx), std::sync::Arc::new(crate::rpc::Offline), crate::sync::Opened::detached(doc), false, ".".into(), app_tx, None);
        assert!(app.prs_summary().is_none(), "nothing tracked, no coordinator: no section");
        let mut pr = TrackedPr::new(PrRef::parse("octo/app#42").unwrap(), Role::Owned, "s1", "Fix flaky test", "", "you", 1);
        pr.dispatches.push(Dispatch { id: "d-1-0001".into(), state: DispatchState::Blocked, reason: "not running".into(), ..Default::default() });
        let registry = Registry { prs: vec![pr] };
        app.bus.prs = Some(PrsView::of("c1", &registry, vec!["PR coordinator".into()], String::new(), Vec::new()));
        assert_eq!(app.prs_summary().as_deref(), Some("1 tracked, 1 active, 1 need you"));
        // Another canvas's view is not this canvas's.
        app.bus.prs.as_mut().unwrap().canvas = "c2".into();
        assert!(app.prs_view().is_none());
        app.bus.prs.as_mut().unwrap().canvas = "c1".into();
        app.run(Action::Prs(PrAction::Panel));
        assert_eq!(app.right_detail, Some(crate::side::RightDetail::Prs));
        app.run(Action::Prs(PrAction::Dismiss("d-1-0001".into())));
        app.run(Action::Prs(PrAction::Check(String::new())));
        app.run(Action::Prs(PrAction::Untrack("https://github.com/octo/app/pull/42".into())));
        assert!(matches!(app.modals.last(), Some(Modal::Confirm(c)) if c.title == "Stop tracking octo/app#42?"), "untracking asks first");
        let requests: Vec<serde_json::Value> = sent.try_iter().filter_map(|m| if let ClientMsg::Bus { json } = m { serde_json::from_str(&json).ok() } else { None }).collect();
        assert!(requests.iter().any(|r| r["action"] == "prs" && r["op"] == "dismiss" && r["dispatch"] == "d-1-0001"), "{requests:?}");
        assert!(requests.iter().any(|r| r["op"] == "check" && r["url"] == ""));
        assert!(!requests.iter().any(|r| r["op"] == "untrack"), "not before you confirm");
        assert!(!sent.try_iter().any(|m| matches!(m, ClientMsg::Launch { .. })), "nothing starts");
    }
}
