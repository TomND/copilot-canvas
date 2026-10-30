//! Review follow-ups on the canvas (WI-016): the panel of the routed
//! canvas's reviews, the messages to their authors (approve before they are
//! sent), the watched conversations with their last and next check, and the
//! authors' requests for another review with what became of each. The
//! service owns the records: the panel shows the bus snapshot's
//! [`ReviewsView`] and its buttons are bus requests (or, for pausing the
//! checks, the agent's settings in the document). Nothing here starts a
//! session but *Resume reviewer*, an explicit start.

use crate::app::App;
use canvas_model::reviews::{MessageState, RequestState, ReviewsView};
use ratatui::style::Color;

use crate::paint::{ACCENT, ATTENTION, MUTED, NEGATIVE, POSITIVE};

/// The marker of review follow-ups.
pub const MARK: &str = "✎";

#[derive(Clone, Debug, PartialEq)]
pub enum ReviewAction {
    /// The review follow-ups panel.
    Panel,
    /// Approve a review's drafted message (it is sent by the agent then).
    Approve(String),
    /// Drop a review's message (nothing is sent).
    DismissMessage(String),
    /// Settle an unclear request with a review (request id, review id).
    Assign(String, String),
    /// Drop a request.
    Dismiss(String),
    /// Stop watching a conversation.
    Unwatch(String),
    /// Ask for a check now.
    CheckNow,
    /// Pause or resume the hourly checks (the agent's setting).
    ToggleChecks,
}

/// A message state's glyph and colour.
pub fn message_look(state: MessageState) -> (&'static str, Color) {
    match state {
        MessageState::Sent => ("✓", POSITIVE),
        MessageState::AwaitingApproval | MessageState::NeedsYou => ("⚠", ATTENTION),
        MessageState::Failed => ("✗", NEGATIVE),
        MessageState::Dismissed => ("·", MUTED),
        MessageState::Pending | MessageState::Approved => ("◌", ACCENT),
    }
}

/// A request state's glyph and colour.
pub fn request_look(state: RequestState) -> (&'static str, Color) {
    match state {
        RequestState::Delivered | RequestState::Done => ("✓", POSITIVE),
        RequestState::Ambiguous | RequestState::Blocked => ("⚠", ATTENTION),
        RequestState::Failed => ("✗", NEGATIVE),
        RequestState::Dismissed => ("·", MUTED),
        RequestState::Intended | RequestState::Queued | RequestState::Waiting => ("◌", ACCENT),
    }
}

impl App {
    /// The routed canvas's review follow-ups (`None`: none and no agent for them).
    pub fn reviews_view(&self) -> Option<&ReviewsView> {
        self.bus.reviews.as_ref().filter(|v| self.doc.canvas_id.as_deref() == Some(v.canvas.as_str()))
    }

    pub fn run_reviews(&mut self, action: ReviewAction) {
        self.needs_draw = true;
        let request = |op: &str, extra: serde_json::Value| {
            let mut value = serde_json::json!({ "action": "reviews", "op": op });
            if let (Some(target), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
                target.extend(extra.clone());
            }
            value
        };
        match action {
            ReviewAction::Panel => self.right_detail = Some(crate::side::RightDetail::Reviews),
            ReviewAction::Approve(review) => {
                self.bus_action(request("approve", serde_json::json!({ "review": review })));
                self.notify("Approved: the review follow-up agent sends it (Reports confirms it).", false);
            }
            ReviewAction::DismissMessage(review) => self.bus_action(request("dismiss-message", serde_json::json!({ "review": review }))),
            ReviewAction::Assign(request_id, review) => self.bus_action(request("assign", serde_json::json!({ "request": request_id, "review": review }))),
            ReviewAction::Dismiss(request_id) => self.bus_action(request("dismiss", serde_json::json!({ "request": request_id }))),
            ReviewAction::Unwatch(conversation) => self.bus_action(request("unwatch", serde_json::json!({ "conversation": conversation }))),
            ReviewAction::CheckNow => {
                self.bus_action(request("check-now", serde_json::json!({})));
                self.notify("A check of the watched conversations is asked for now.", false);
            }
            ReviewAction::ToggleChecks => {
                let paused = self.doc.nodes.iter_mut().filter(|n| n.passive.as_ref().is_some_and(|p| p.is_canvas() && p.reviews.is_some())).find_map(|n| {
                    let settings = n.passive.as_mut()?.reviews.as_mut()?;
                    settings.paused = !settings.paused;
                    Some(settings.paused)
                });
                match paused {
                    Some(paused) => {
                        self.mark_dirty();
                        self.sync_routing();
                        self.notify(if paused { "Checks paused: no conversation is read until you resume them." } else { "Checks resumed." }, false);
                    }
                    None => self.notify("No canvas-wide agent has review follow-ups.", true),
                }
            }
        }
    }

    /// What the agents panel and an agent's panel say about review follow-ups.
    pub fn reviews_summary(&self) -> Option<String> {
        let view = self.reviews_view()?;
        let watched = view.conversations.iter().filter(|c| c.watching).count();
        let mut text = format!("{} review(s), {watched} watched conversation(s)", view.reviews.len());
        if view.needing_you() > 0 {
            text.push_str(&format!(", {} need you", view.needing_you()));
        }
        if !view.paused.is_empty() {
            text.push_str(" · checks off");
        }
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialogs::Action;
    use crate::doc::{Doc, Node, Passive, PassiveScope};
    use crate::protocol::ClientMsg;
    use canvas_model::reviews::{Book, ReviewSettings};

    #[test]
    fn the_panels_buttons_are_service_requests_and_pausing_is_the_agents_setting() {
        let mut doc = Doc { canvas_id: Some("c1".into()), ..Doc::default() };
        let mut agent: Node = serde_json::from_value(serde_json::json!({ "id": "f", "kind": "session", "name": "Review follow-up", "x": 0, "y": 0, "agent": "copilot" })).unwrap();
        agent.source = Some(crate::doc::CANVAS_AGENT.into());
        agent.passive = Some(Passive { key: "kf".into(), scope: PassiveScope::Canvas, reviews: Some(ReviewSettings::default()), ..Default::default() });
        doc.nodes.push(agent);
        let (tx, sent) = crossbeam_channel::bounded(256);
        let (_service, service_rx) = crossbeam_channel::bounded(1);
        let (app_tx, _app_rx) = crossbeam_channel::unbounded();
        let mut app = App::new(crate::conn::Conn::detached(tx, service_rx), std::sync::Arc::new(crate::rpc::Offline), crate::sync::Opened::detached(doc), false, ".".into(), app_tx, None);
        app.connected = true;
        app.bus.reviews = Some(ReviewsView::of("c1", &Book::default(), vec!["Review follow-up".into()], Some(ReviewSettings::default()), String::new()));
        assert_eq!(app.reviews_summary().as_deref(), Some("0 review(s), 0 watched conversation(s)"));
        app.run(Action::Reviews(ReviewAction::Panel));
        assert_eq!(app.right_detail, Some(crate::side::RightDetail::Reviews));
        app.run(Action::Reviews(ReviewAction::Approve("rv-1".into())));
        app.run(Action::Reviews(ReviewAction::Assign("q-1".into(), "rv-2".into())));
        app.run(Action::Reviews(ReviewAction::ToggleChecks));
        assert!(app.doc.node("f").unwrap().passive.as_ref().unwrap().reviews.as_ref().unwrap().paused, "pausing is the agent's setting");
        let messages: Vec<ClientMsg> = sent.try_iter().collect();
        let requests: Vec<serde_json::Value> = messages.iter().filter_map(|m| if let ClientMsg::Bus { json } = m { serde_json::from_str(json).ok() } else { None }).collect();
        assert!(requests.iter().any(|r| r["action"] == "reviews" && r["op"] == "approve" && r["review"] == "rv-1"), "{requests:?}");
        assert!(requests.iter().any(|r| r["op"] == "assign" && r["request"] == "q-1" && r["review"] == "rv-2"));
        assert!(messages.iter().any(|m| matches!(m, ClientMsg::Routing { json } if json.contains("\"paused\":true"))), "the routing carries the paused checks");
        assert!(!messages.iter().any(|m| matches!(m, ClientMsg::Launch { .. })), "nothing starts");
    }
}
