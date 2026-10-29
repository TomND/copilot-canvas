//! The reports inbox in the service: stores what `copilot-canvas report
//! publish` spooled (looked at every 250 ms), keeps the read state, and
//! sends the inbox to every client that shows it (`ServiceMsg::Reports`, on
//! connect and on every change). Clients change the read state with bus
//! requests (`{"action": "reports", "op": "read" | "unread" | "read_all" |
//! "delete", "ids": […]}`), which older services ignore.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::{Shared, lock, log};
use crate::paths::now_ms;
use crate::protocol::ServiceMsg;

/// How often the spool is looked at.
const POLL: Duration = Duration::from_millis(250);

/// The inbox as a message for clients.
pub(super) fn message(shared: &Shared) -> ServiceMsg {
    let snapshot = lock(&shared.reports).snapshot();
    ServiceMsg::Reports { json: serde_json::to_string(&snapshot).unwrap_or_default() }
}

pub(super) fn run(shared: Arc<Shared>) {
    let home = shared.instance.home.clone();
    if let Err(error) = crate::reports::announce_service(&home) {
        log(&shared.instance, &format!("reports: cannot announce the service to the CLI ({error:#}); reports will say queued"));
    }
    loop {
        std::thread::sleep(POLL);
        if shared.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        let done = {
            let routing = lock(&shared.routing);
            lock(&shared.reports).ingest(&routing, now_ms())
        };
        for report in &done.stored {
            log(&shared.instance, &format!("report {} stored: {:?} from {} ({}, {}, {} bytes)", report.id, report.title, report.source_name, report.format.label(), report.kind, report.size));
        }
        for rejected in &done.rejected {
            log(&shared.instance, &format!("report {} refused: {}", rejected.id, rejected.reason));
        }
        for (id, error) in &done.failed {
            log(&shared.instance, &format!("report {id} not stored yet (tried again in 30 s): {error}"));
        }
        if done.changed() {
            shared.broadcast(message(&shared));
        }
    }
}

/// A client's request about the inbox; false when `value` is none.
pub(super) fn action(shared: &Shared, value: &serde_json::Value) -> bool {
    if value["action"].as_str() != Some("reports") {
        return false;
    }
    let ids: Vec<String> = value["ids"].as_array().map(|ids| ids.iter().filter_map(|i| i.as_str().map(str::to_owned)).collect()).unwrap_or_default();
    let now = now_ms();
    let changed = {
        let mut store = lock(&shared.reports);
        match value["op"].as_str().unwrap_or_default() {
            "read" => store.mark(&ids, true, now),
            "unread" => store.mark(&ids, false, now),
            "read_all" => store.mark_all_read(now),
            "delete" => {
                let gone = store.delete(&ids);
                if !gone.is_empty() {
                    log(&shared.instance, &format!("reports deleted by a client: {}", gone.join(", ")));
                }
                !gone.is_empty()
            }
            _ => false,
        }
    };
    if changed {
        shared.broadcast(message(shared));
    }
    true
}
