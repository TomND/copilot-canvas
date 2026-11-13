//! The bus snapshot is sent when it changes, not every second (S8-01): a
//! service in a temporary home with one passive session; an event waits out
//! its 8 s debounce, during which the snapshot used to change every second
//! (`next_run_ms` was the clock) and was broadcast whole each time.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use canvas_common::paths::Instance;
use canvas_common::protocol::{ClientMsg, ServiceMsg};
use canvas_core::conn::{self, Conn};

struct Sandbox {
    root: PathBuf,
    instance: Instance,
}

impl Sandbox {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("cc-bus-quiet-{}", uuid::Uuid::new_v4()));
        for dir in [root.join("home"), root.join("copilot").join("session-state")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let instance = Instance::new(Some(format!("busquiet{}", std::process::id())), Some(root.join("home")), Some(root.join("copilot").join("session-state"))).unwrap();
        Sandbox { root, instance }
    }

    fn start(&self) -> Conn {
        let home = self.root.join("copilot").display().to_string();
        let env = [("COPILOT_HOME", home.as_str()), ("ATELIER_FAKE_COPILOT", "1"), ("ATELIER_SELFTEST", "1")];
        conn::spawn_service_exe(Path::new(env!("CARGO_BIN_EXE_atelier")), &self.instance, &env).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match conn::try_connect(&self.instance) {
                Ok(conn) => return conn,
                Err(error) => {
                    assert!(Instant::now() < deadline, "cannot connect: {error:#}");
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Ok(conn) = conn::try_connect(&self.instance) {
            let _ = conn.tx.send(ClientMsg::Shutdown);
            let deadline = Instant::now() + Duration::from_secs(15);
            while conn::service_answers(&self.instance) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        for _ in 0..20 {
            if std::fs::remove_dir_all(&self.root).is_ok() || !self.root.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

/// The bus snapshots `conn` gets within `wait`.
fn snapshots(conn: &Conn, wait: Duration) -> Vec<canvas_model::bus::Snapshot> {
    let deadline = Instant::now() + wait;
    let mut got = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if let Ok(ServiceMsg::Bus { json }) = conn.rx.recv_timeout(left.min(Duration::from_millis(200))) {
            got.push(serde_json::from_str(&json).unwrap());
        }
    }
    got
}

#[test]
fn the_bus_snapshot_is_sent_when_it_changes_not_every_second() {
    let sandbox = Sandbox::new();
    let conn = sandbox.start();
    // One group and a passive session on its events (the group has no folder: a run would fail, later).
    let routing = serde_json::json!({
        "groups": [{ "id": "g1", "name": "Pay", "folder": null }],
        "members": {},
        "names": {},
        "no_messages": [],
        "passive": [{
            "key": "k1", "session_id": "11111111-2222-3333-4444-555555555555", "name": "Watch", "group": "g1", "slug": "watch",
            "subscribe": ["test.*"], "filter": "", "every_minutes": 0, "daily_at": "", "paused": false,
            "rollover_runs": 0, "max_runs_per_day": 0, "mcp_off": []
        }]
    });
    conn.tx.send(ClientMsg::Routing { json: routing.to_string() }).unwrap();
    let settled = snapshots(&conn, Duration::from_secs(3));
    assert!(settled.last().is_some_and(|s| s.passive.contains_key("k1")), "the passive session is listed");
    // An event for it: it waits out the 8 s debounce.
    let emit = serde_json::json!({ "action": "emit", "event": { "type": "test.ping", "group": "g1" } });
    conn.tx.send(ClientMsg::Bus { json: emit.to_string() }).unwrap();
    let during = snapshots(&conn, Duration::from_secs(6));
    assert!(during.first().is_some_and(|s| s.passive["k1"].pending == 1), "the event is in the first snapshot after it");
    assert!(during.len() <= 2, "one snapshot per change, not one a second: {} in 6 s", during.len());
    assert!(during.iter().all(|s| s.passive["k1"].next_run_ms == during[0].passive["k1"].next_run_ms), "the next run does not move with the clock");
}
