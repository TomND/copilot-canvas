//! Restore after a restart, end to end: a service in a temporary home runs
//! two stand-in Copilot sessions (`src/fake_copilot.rs`); you stop one, the
//! other is working when the service is killed. Started again, the service
//! resumes the working one by itself (no client: `--session-id` with
//! resume, the same id), types the restore note into it once it has settled,
//! and leaves the stopped one alone. Then a graceful `service stop` keeps it
//! for the next start too.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use canvas_common::paths::Instance;
use canvas_common::protocol::{AgentLaunch, ClientMsg, LaunchKind, ServiceMsg};
use canvas_core::conn::{self, Conn};

struct Sandbox {
    root: PathBuf,
    instance: Instance,
    copilot: PathBuf,
    copilot_home: PathBuf,
    work: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("cc-restore-e2e-{}", uuid::Uuid::new_v4()));
        let (bin, copilot_home, work) = (root.join("bin"), root.join("copilot"), root.join("work"));
        for dir in [&bin, &copilot_home.join("session-state"), &work, &root.join("home")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        // The stand-in: this package's binary under the agent's name.
        let copilot = bin.join(format!("copilot{}", std::env::consts::EXE_SUFFIX));
        let exe = Path::new(env!("CARGO_BIN_EXE_atelier"));
        if std::fs::hard_link(exe, &copilot).is_err() {
            std::fs::copy(exe, &copilot).unwrap();
        }
        let name = format!("restore{}", std::process::id());
        let instance = Instance::new(Some(name), Some(root.join("home")), Some(copilot_home.join("session-state"))).unwrap();
        Sandbox { root, instance, copilot, copilot_home, work }
    }

    fn start(&self) {
        let copilot = self.copilot.display().to_string();
        let home = self.copilot_home.display().to_string();
        let env = [("ATELIER_COPILOT", copilot.as_str()), ("COPILOT_HOME", home.as_str()), ("ATELIER_FAKE_COPILOT", "1"), ("ATELIER_FAKE_HISTORY", "1"), ("ATELIER_SELFTEST", "1")];
        conn::spawn_service_exe(Path::new(env!("CARGO_BIN_EXE_atelier")), &self.instance, &env).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !conn::service_answers(&self.instance) {
            assert!(Instant::now() < deadline, "the service did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn connect(&self) -> Conn {
        let deadline = Instant::now() + Duration::from_secs(20);
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

    fn log(&self) -> String {
        std::fs::read_to_string(self.instance.log_path()).unwrap_or_default()
    }

    /// Waits until the service log (from `from` on) satisfies `ok`.
    fn wait_log(&self, what: &str, from: usize, secs: u64, ok: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let log = self.log();
            let tail = log.get(from..).unwrap_or_default().to_owned();
            if ok(&tail) {
                return tail;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}; the service log since:\n{tail}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn running_set(&self) -> serde_json::Value {
        let path = canvas_agents::service::restore::path(&self.instance.home, &self.instance.name);
        std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    fn events(&self, id: &str) -> String {
        std::fs::read_to_string(self.copilot_home.join("session-state").join(id).join("events.jsonl")).unwrap_or_default()
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

fn launch(conn: &Conn, id: &str, name: &str, cwd: &Path) {
    let launch = AgentLaunch { agent: "copilot".into(), name: Some(name.into()), ..Default::default() };
    conn.tx.send(ClientMsg::Launch { id: id.into(), kind: LaunchKind::Agent(Box::new(launch)), cwd: cwd.display().to_string(), cols: 100, rows: 30 }).unwrap();
}

/// Waits until terminal `id` is alive (or not).
fn wait_alive(conn: &Conn, id: &str, alive: bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "terminal {id} never became alive={alive}");
        match conn.rx.recv_timeout(Duration::from_millis(200)) {
            Ok(ServiceMsg::Terminals(infos)) if infos.iter().any(|i| i.id == id && i.alive == alive) => return,
            Ok(ServiceMsg::Error { message, .. }) => panic!("the service refused: {message}"),
            _ => {}
        }
    }
}

fn service_pid(conn: &Conn) -> u32 {
    conn.tx.send(ClientMsg::Stats).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "no stats");
        if let Ok(ServiceMsg::Stats(stats)) = conn.rx.recv_timeout(Duration::from_millis(200)) {
            return stats.pid;
        }
    }
}

fn kill(pid: u32) {
    let status = if cfg!(windows) {
        std::process::Command::new("taskkill").args(["/F", "/PID", &pid.to_string()]).status()
    } else {
        std::process::Command::new("kill").args(["-9", &pid.to_string()]).status()
    };
    assert!(status.is_ok_and(|s| s.success()), "cannot kill the service");
}

#[test]
fn a_killed_service_resumes_its_sessions_with_a_note_and_leaves_stopped_ones_alone() {
    let sandbox = Sandbox::new();
    let (working, stopped) = (uuid::Uuid::new_v4().to_string(), uuid::Uuid::new_v4().to_string());
    sandbox.start();
    let conn = sandbox.connect();
    launch(&conn, &working, "Working", &sandbox.work);
    wait_alive(&conn, &working, true);
    launch(&conn, &stopped, "Stopped", &sandbox.work);
    wait_alive(&conn, &stopped, true);
    // You stop one (a graceful stop): it leaves the running set.
    conn.tx.send(ClientMsg::Stop { id: stopped.clone(), force: false }).unwrap();
    wait_alive(&conn, &stopped, false);
    // The other is in the middle of a turn (its history says so).
    let history = sandbox.copilot_home.join("session-state").join(&working).join("events.jsonl");
    let line = format!("{{\"type\":\"user.message\",\"timestamp\":\"{}\",\"data\":{{\"content\":\"a long task\"}}}}\n", canvas_common::paths::rfc3339_now());
    std::fs::OpenOptions::new().append(true).open(&history).and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes())).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let set = sandbox.running_set();
        let entries = &set["entries"];
        if entries[&working]["doing"]["working"] == true && entries.get(&stopped).is_none() {
            break;
        }
        assert!(Instant::now() < deadline, "the running set never said it works: {set}\n{}", sandbox.log());
        std::thread::sleep(Duration::from_millis(200));
    }
    // The service dies (a crash, a killed process).
    kill(service_pid(&conn));
    drop(conn);
    let deadline = Instant::now() + Duration::from_secs(15);
    while conn::service_answers(&sandbox.instance) {
        assert!(Instant::now() < deadline, "the killed service still answers");
        std::thread::sleep(Duration::from_millis(100));
    }
    let before = sandbox.log().len();
    // Started again with no client: it resumes the working session by itself.
    sandbox.start();
    let log = sandbox.wait_log("the resume", before, 30, |log| log.contains(&format!("terminal {working}: copilot resume")) && log.contains(&format!("launched {working}")));
    assert!(log.contains(&format!("restore: {working} resumed copilot (it was working)")), "{log}");
    assert!(!log.contains(&stopped), "the session you stopped is not resumed:\n{log}");
    // Once it has settled, the note is typed into it (the stand-in records the turn).
    let typed_before = sandbox.events(&working).matches("user.message").count();
    let log = sandbox.wait_log("the note", before, 45, |log| log.contains(&format!("delivered a message from Atelier (the restore note) to {working}")));
    assert!(log.contains(&format!("restore: {working} has settled; its note is queued")), "{log}");
    let deadline = Instant::now() + Duration::from_secs(15);
    while sandbox.events(&working).matches("user.message").count() <= typed_before {
        assert!(Instant::now() < deadline, "the note never reached the session: {}", sandbox.events(&working));
        std::thread::sleep(Duration::from_millis(100));
    }
    // A client sees it running, and the bus snapshot says it was resumed.
    let conn = sandbox.connect();
    let deadline = Instant::now() + Duration::from_secs(15);
    let (mut alive, mut resumed) = (false, false);
    while !(alive && resumed) {
        assert!(Instant::now() < deadline, "the client never saw the resumed session (alive {alive}, resumed {resumed})");
        match conn.rx.recv_timeout(Duration::from_millis(200)) {
            Ok(ServiceMsg::Terminals(infos)) => alive |= infos.iter().any(|i| i.id == working && i.alive),
            Ok(ServiceMsg::Bus { json }) => {
                let snapshot: canvas_model::bus::Snapshot = serde_json::from_str(&json).unwrap();
                resumed |= snapshot.restore.is_some_and(|r| r.resumed.contains(&working) && !r.is_resuming(&working));
            }
            _ => {}
        }
    }
    // A graceful stop of the service keeps it for the next start.
    conn.tx.send(ClientMsg::Shutdown).unwrap();
    drop(conn);
    let deadline = Instant::now() + Duration::from_secs(20);
    while conn::service_answers(&sandbox.instance) {
        assert!(Instant::now() < deadline, "the service did not stop");
        std::thread::sleep(Duration::from_millis(100));
    }
    let set = sandbox.running_set();
    assert!(set["entries"].get(&working).is_some() && set["entries"].get(&stopped).is_none(), "{set}");
}
