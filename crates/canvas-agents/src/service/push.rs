//! Push notifications to paired phones (docs/IOS-PLAN.md, phase 5; the
//! sending is `canvas_push`). Phones register with `ClientMsg::PushRegister`;
//! while a phone is not connected, the service tells it when a session starts
//! waiting for you or finishes a turn and when a report arrives in the inbox,
//! and sends it renewed tunnel access tokens (sealed with its pairing secret)
//! so it can still connect after a day away. Needs the team's APNs key
//! (`copilot-canvas remote push-key`).

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::Result;
use canvas_model::agent::native_id;
use canvas_model::doc::{Doc, NodeKind};
use canvas_model::status::{Status, session_status};
use canvas_push::{Attention, Device, Devices, Outcome, Sender};

use super::{Agents, Client, lock, log};

pub(super) struct Push {
    pub devices: Devices,
    /// The sender for the key file as of its modification time.
    sender: Mutex<Option<(SystemTime, Arc<Sender>)>>,
    attention: Mutex<Attention>,
    /// The canvas as of the file's modification time (names, review times).
    doc: Mutex<Option<(SystemTime, Doc)>>,
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl Push {
    pub fn new(home: &Path) -> Self {
        Self { devices: Devices::open(home), sender: Mutex::new(None), attention: Mutex::new(Attention::default()), doc: Mutex::new(None) }
    }

    /// The sender for `<home>/remote/apns.json` (reloaded when it changes); `None` without a key.
    pub fn sender(&self, home: &Path) -> Option<Arc<Sender>> {
        let path = canvas_push::ApnsKey::path(home);
        let stamp = modified(&path)?;
        let mut current = lock(&self.sender);
        if let Some((at, sender)) = current.as_ref()
            && *at == stamp
        {
            return Some(sender.clone());
        }
        let sender = Arc::new(Sender::new(canvas_push::ApnsKey::load(home)?).ok()?);
        *current = Some((stamp, sender.clone()));
        Some(sender)
    }

    fn doc(&self, home: &Path) -> Option<Doc> {
        let path = home.join("canvas.json");
        let stamp = modified(&path)?;
        let mut cached = lock(&self.doc);
        if cached.as_ref().is_none_or(|(at, _)| *at != stamp) {
            let doc: Doc = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
            *cached = Some((stamp, doc));
        }
        cached.as_ref().map(|(_, doc)| doc.clone())
    }
}

/// A phone registered (or, with an empty token, unregistered) for notifications.
pub(super) fn register(agents: &Agents, client: &Arc<Client>, token: &str, environment: &str, bundle: &str, label: &str) -> Result<()> {
    let Some(client_id) = client.remote_id.as_deref() else { anyhow::bail!("only paired phones receive notifications") };
    if token.is_empty() {
        agents.push.devices.remove(Some(client_id), None);
        log(&agents.instance, &format!("push: {client_id} unregistered"));
        return Ok(());
    }
    let device = Device::new(client_id, token, environment, bundle, label)?;
    if agents.push.devices.register(device.clone()) {
        let key = if agents.push.sender(&agents.instance.home).is_some() { "" } else { " (no APNs key yet: `copilot-canvas remote push-key`)" };
        log(&agents.instance, &format!("push: {client_id} registered for notifications ({:?}, as {:?}){key}", device.environment, device.label));
    }
    // A phone that wants notifications also wants tunnel tokens while away.
    let _ = agents.tunnel_wake.try_send(());
    Ok(())
}

/// Paired clients connected now (they show everything themselves).
fn connected(agents: &Agents) -> HashSet<String> {
    lock(&agents.clients).values().filter_map(|c| c.remote_id.clone()).collect()
}

/// Every session box's status, by the clients' own rules.
fn statuses(agents: &Agents, doc: &Doc) -> Vec<(String, String, Status)> {
    let sessions = lock(&agents.sessions).clone();
    let natives = lock(&agents.agent_sessions).clone();
    let terms: std::collections::HashMap<String, canvas_common::protocol::TermInfo> = lock(&agents.terms).iter().map(|(id, t)| (id.clone(), t.info())).collect();
    doc.nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Session && n.passive.is_none())
        .map(|node| {
            let native = natives.get(&node.id).map(String::as_str).unwrap_or_else(|| native_id(node));
            let status = session_status(sessions.get(native), terms.get(&node.id), node.reviewed_ms);
            (node.id.clone(), node.name.clone(), status)
        })
        .collect()
}

/// Sends `payload` to each device (on its own thread); forgets tokens APNs refuses.
fn send_all(agents: &Arc<Agents>, sender: Arc<Sender>, jobs: Vec<(Device, serde_json::Value, bool, Option<String>, String)>) {
    if jobs.is_empty() {
        return;
    }
    let agents = agents.clone();
    std::thread::spawn(move || {
        for (device, payload, background, collapse, what) in jobs {
            match sender.send(&device, &payload, background, collapse.as_deref()) {
                Outcome::Sent => log(&agents.instance, &format!("push: sent {what} to {}", device.client_id)),
                Outcome::Unregistered => {
                    log(&agents.instance, &format!("push: {} no longer takes notifications (app removed?); forgotten", device.client_id));
                    agents.push.devices.remove(None, Some(&device.token));
                }
                Outcome::Failed(error) => log(&agents.instance, &format!("push: {what} to {} failed: {error}", device.client_id)),
            }
        }
    });
}

/// Watches the sessions every 2 s and notifies phones that are away.
pub(super) fn push_loop(agents: Arc<Agents>) {
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let home = agents.instance.home.clone();
        let Some(doc) = agents.push.doc(&home) else { continue };
        let events = lock(&agents.push.attention).update(statuses(&agents, &doc));
        if events.is_empty() || agents.push.devices.is_empty() {
            continue;
        }
        let Some(sender) = agents.push.sender(&home) else { continue };
        let online = connected(&agents);
        let jobs = agents
            .push
            .devices
            .list()
            .into_iter()
            .filter(|d| !online.contains(&d.client_id) && agents.remote.secret(&d.client_id).is_some())
            .flat_map(|device| {
                events.iter().map(move |event| {
                    let what = format!("{:?} for {:?}", if event.status == Status::Waiting { "needs you" } else { "finished" }, event.name);
                    (device.clone(), canvas_push::alert_payload(&device, event), false, Some(event.id.clone()), what)
                })
            })
            .collect();
        send_all(&agents, sender, jobs);
    }
}

/// Reports at most notified at once (the newest; the last one says how many more).
const REPORTS_AT_ONCE: usize = 3;

/// Reports the inbox just stored, for the registered phones that are away.
pub(super) fn reports(agents: &Arc<Agents>, stored: &[canvas_model::reports::Report]) {
    if stored.is_empty() || agents.push.devices.is_empty() {
        return;
    }
    let Some(sender) = agents.push.sender(&agents.instance.home) else { return };
    let online = connected(agents);
    // Stored oldest first: the newest few, the newest last (it lands on top).
    let shown: Vec<&canvas_model::reports::Report> = stored.iter().rev().take(REPORTS_AT_ONCE).rev().collect();
    let more = stored.len() - shown.len();
    let mut jobs = Vec::new();
    for device in agents.push.devices.list().into_iter().filter(|d| !online.contains(&d.client_id) && agents.remote.secret(&d.client_id).is_some()) {
        for (i, report) in shown.iter().enumerate() {
            let extra = if i + 1 == shown.len() { more } else { 0 };
            let collapse = if report.thread.is_empty() { report.id.clone() } else { format!("r:{}", report.thread) };
            jobs.push((device.clone(), canvas_push::report_payload(&device, report, extra), false, Some(collapse), format!("the report {:?}", report.title)));
        }
    }
    send_all(agents, sender, jobs);
}

/// Whether away phones want tunnel tokens (registered, and a key to reach them).
pub(super) fn wants_access(agents: &Agents) -> bool {
    !agents.push.devices.is_empty() && agents.push.sender(&agents.instance.home).is_some()
}

/// A renewed tunnel token for the registered phones that are away, sealed for each.
pub(super) fn send_access(agents: &Arc<Agents>, access: &crate::tunnel_access::Access) {
    let Some(sender) = agents.push.sender(&agents.instance.home) else { return };
    let online = connected(agents);
    let jobs = agents
        .push
        .devices
        .list()
        .into_iter()
        .filter(|d| !online.contains(&d.client_id))
        .filter_map(|device| {
            let secret = agents.remote.secret(&device.client_id)?;
            let payload = canvas_push::access_payload(&device, &secret, &access.url, &access.token, access.expires_ms);
            Some((device, payload, true, None, "a renewed tunnel token".to_owned()))
        })
        .collect();
    send_all(agents, sender, jobs);
}
