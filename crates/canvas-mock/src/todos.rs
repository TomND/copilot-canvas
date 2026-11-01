//! The mock's todo boards (docs/NOTEBOOK-TASKS.md §3): the demo book
//! (`todos::demo_book`) on the mock's canvas (the "Canvas App" board linked
//! to the Payments group, its claims by the mock's sessions), sent in every
//! bus snapshot, and the windows' todo actions (`{"action": "todos", "op":
//! …}`) applied as you, as the service applies them. In memory: every start
//! begins with the demo book.

use canvas_model::bus;
use canvas_model::todos::{self, Actor, Ctx, TodoBook, TodoOp};

/// The demo book with the mock's group and sessions.
pub fn demo() -> TodoBook {
    let mut book = todos::demo_book();
    for group in book.boards.iter_mut().flat_map(|b| b.canvas_groups.iter_mut()) {
        if group == "g-demo" {
            *group = "g-payments".into();
        }
    }
    let recast = |actor: &mut Actor| {
        let (session, name) = match actor.session.as_deref() {
            Some("demo-main") => ("mock-fix", "Fix flaky test"),
            Some("demo-review") => ("mock-review", "Review PR #42"),
            _ => return,
        };
        actor.session = Some(session.into());
        actor.name = Some(name.into());
    };
    for todo in &mut book.todos {
        recast(&mut todo.created_by);
        if let Some(claim) = todo.claimed_by.as_mut() {
            recast(&mut claim.by);
        }
        for entry in &mut todo.log {
            recast(&mut entry.by);
        }
    }
    book
}

/// The bus snapshot clients get: the mock has no passive sessions' views, only the book.
pub fn snapshot(book: &TodoBook) -> String {
    serde_json::to_string(&bus::Snapshot { todos: Some(book.clone()), ..Default::default() }).unwrap_or_default()
}

/// A window's todo action (`None`: not one), applied as you: what happened,
/// or the notice for that window when it is refused.
pub fn action(book: &mut TodoBook, value: &serde_json::Value, ctx: &Ctx) -> Option<Result<String, String>> {
    if value["action"].as_str() != Some("todos") {
        return None;
    }
    let op = match serde_json::from_value::<TodoOp>(value["op"].clone()) {
        Ok(op) => op,
        Err(error) => return Some(Err(format!("Todo change not understood by the service ({error})"))),
    };
    Some(book.apply(op, &Actor::human(), ctx).map(|applied| applied.message).map_err(|why| format!("Todo change refused: {why}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_demo_book_lives_on_the_mocks_canvas() {
        let book = demo();
        assert_eq!(book.find_board("Canvas App").map(|b| b.canvas_groups.clone()), Some(vec!["g-payments".to_owned()]));
        assert_eq!(book.claimed_by_session("mock-fix").map(|t| t.id.as_str()), Some("WI-002"));
        assert_eq!(book.todo("WI-003").and_then(|t| t.claimed_by.as_ref()).and_then(|c| c.by.name.clone()).as_deref(), Some("Review PR #42"));
        assert!(book.todos.iter().all(|t| t.created_by.session.as_deref() != Some("demo-main") && t.log.iter().all(|e| e.by.session.as_deref() != Some("demo-main"))));
        assert!(book.group_counts("g-payments").is_some_and(|c| c.proposed == 1));
        let sent: bus::Snapshot = serde_json::from_str(&snapshot(&book)).unwrap();
        assert_eq!(sent.todos.as_ref(), Some(&book));
    }

    #[test]
    fn window_actions_apply_as_you_and_refusals_say_why() {
        let mut book = demo();
        let ctx = Ctx::at(1_800_000_000_000);
        let accept = todos::client_action(&TodoOp::Accept { id: "WI-006".into(), status: None });
        assert!(action(&mut book, &accept, &ctx).is_some_and(|r| r.is_ok_and(|m| m.starts_with("Accepted WI-006"))));
        assert_eq!(book.todo("WI-006").map(|t| t.status), Some(todos::Status::Backlog));
        let again = action(&mut book, &accept, &ctx).unwrap().unwrap_err();
        assert!(again.starts_with("Todo change refused: WI-006 is not a proposal"), "{again}");
        let garbled = serde_json::json!({ "action": "todos", "op": { "op": "levitate" } });
        assert!(action(&mut book, &garbled, &ctx).unwrap().unwrap_err().contains("not understood"));
        assert!(action(&mut book, &serde_json::json!({ "action": "reports", "op": "read" }), &ctx).is_none());
    }
}
