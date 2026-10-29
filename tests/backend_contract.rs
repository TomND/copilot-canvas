//! Backend contract tests (PLAN.md §10): the same requests against the
//! in-process backend and a real service over loopback TCP give the same
//! answers; two clients editing one canvas through the service converge.
//!
//! The service is this package's binary, started in a temporary home with
//! `COPILOT_CANVAS_LISTEN=127.0.0.1:0` (like `selftest --remote loopback`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use canvas_agents::backend::{Ctx, LocalBackend};
use canvas_common::paths::Instance;
use canvas_common::protocol::{ClientMsg, ServiceMsg};
use canvas_core::conn::{self, ClientSession, Conn, ConnOptions, ConnTarget};
use canvas_core::remote::RemoteBackend;
use canvas_model::agent::AgentKind;
use canvas_model::doc::{Doc, Group, Node};
use canvas_model::rpc::{Backend, PassiveFiles};
use canvas_model::sync::{self, Op, Replica};

static COPILOT_HOME: Once = Once::new();

/// Every backend in this process and the services it starts read Copilot's
/// files (the sample automation database) from one temporary folder.
fn copilot_home() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cc-contract-copilot-{}", std::process::id()));
    COPILOT_HOME.call_once(|| {
        std::fs::create_dir_all(&dir).unwrap();
        canvas_agents::automations::write_sample_db(&dir.join("data.db")).unwrap();
        // SAFETY: set once, before any thread of this process reads it.
        unsafe { std::env::set_var("COPILOT_HOME", &dir) };
    });
    dir
}

/// A real service in a temporary home, listening on loopback TCP.
struct Service {
    instance: Instance,
    root: PathBuf,
}

impl Service {
    fn start(name: &str) -> Service {
        copilot_home();
        let root = std::env::temp_dir().join(format!("cc-contract-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("sessions")).unwrap();
        let instance = Instance::new(Some(format!("{name}{}", std::process::id())), Some(root.join("home")), Some(root.join("sessions"))).unwrap();
        let exe = Path::new(env!("CARGO_BIN_EXE_copilot-canvas"));
        conn::spawn_service_exe(exe, &instance, &[("COPILOT_CANVAS_LISTEN", "127.0.0.1:0")]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while conn::listening_port(&instance).is_none() {
            assert!(Instant::now() < deadline, "the service did not start listening");
            std::thread::sleep(Duration::from_millis(50));
        }
        Service { instance, root }
    }

    /// A protocol-8 client over loopback TCP whose backend calls go to the service.
    fn connect(&self) -> (Conn, Arc<RemoteBackend>) {
        let backend = RemoteBackend::new();
        let options = ConnOptions { name: "contract".into(), rpc: Some(backend.clone()), ..Default::default() };
        let target = ConnTarget::Loopback(self.instance.clone());
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match conn::open(&target, &options, &ClientSession::new()) {
                Ok(conn) => return (conn, backend),
                Err(error) => {
                    assert!(Instant::now() < deadline, "cannot connect: {error:#}");
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }

    fn ctx(&self) -> Ctx {
        Ctx::new(&self.instance)
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Ok(conn) = conn::try_connect(&self.instance) {
            let _ = conn.tx.send(ClientMsg::Shutdown);
            std::thread::sleep(Duration::from_millis(800));
        }
        for _ in 0..20 {
            if std::fs::remove_dir_all(&self.root).is_ok() || !self.root.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// One run of the contract against `backend`; files it creates go under
/// `work/<tag>` so the two runs never collide. Returns the answers both
/// backends must agree on.
fn contract(backend: &dyn Backend, ctx: &Ctx, work: &Path, tag: &str) -> Vec<String> {
    let mut answers = Vec::new();
    let mine = work.join(tag);
    std::fs::create_dir_all(&mine).unwrap();

    // Machine facts.
    let info = backend.info().unwrap();
    assert_eq!(PathBuf::from(&info.home), ctx.home);
    assert_eq!(PathBuf::from(&info.sessions_dir), ctx.sessions_dir);
    assert_eq!(PathBuf::from(&info.automation_db), copilot_home().join("data.db"));
    answers.push(format!("{info:?}"));

    // Folder browsing.
    let listing = backend.list_dir(Some(work.join("shared").display().to_string())).unwrap();
    assert_eq!(listing, ["..", "Alpha", "beta"]);
    answers.push(format!("{listing:?}"));
    let drives = backend.list_dir(None).unwrap();
    assert!(drives.iter().any(|d| d.ends_with(":\\")));
    answers.push(format!("{drives:?}"));
    let folder = backend.path_info(&work.join("shared").display().to_string()).unwrap();
    assert!(folder.exists && folder.dir && folder.occupied && !folder.file);
    let empty = backend.path_info(&work.join("shared").join("beta").display().to_string()).unwrap();
    assert!(empty.dir && !empty.occupied);
    let missing = backend.path_info(&work.join("nope").display().to_string()).unwrap();
    assert!(!missing.exists && !missing.occupied);
    answers.push(format!("{folder:?} {empty:?} {missing:?}"));
    let exist = backend.dirs_exist(vec![work.display().to_string(), work.join("nope").display().to_string()]).unwrap();
    assert_eq!(exist, [true, false]);
    assert!(backend.list_dir(Some(work.join("nope").display().to_string())).is_err());

    // Text viewer and log tails.
    let text = work.join("shared").join("notes.txt");
    assert_eq!(backend.read_text(&text.display().to_string(), 5).unwrap(), "line1");
    assert_eq!(backend.tails(vec![(text.display().to_string(), 8)]).unwrap(), ["line3\n"]);
    assert!(backend.read_text(&work.join("nope.txt").display().to_string(), 10).is_err());
    let newest = backend.newest_matches(vec![work.join("shared").join("*.txt").display().to_string(), work.join("none*").display().to_string()]).unwrap();
    assert_eq!(newest, [text.display().to_string()]);

    // Git: probe, then a worktree on a new branch.
    let repo = work.join("repo");
    let probe = backend.git_probe(&repo.display().to_string()).unwrap().expect("a repository");
    assert_eq!((probe.branch.as_deref(), probe.branches.first().map(String::as_str)), (Some("main"), Some("main")));
    answers.push(format!("{:?} {:?}", probe.root, probe.branch));
    assert_eq!(backend.git_probe(&mine.display().to_string()).unwrap(), None, "a plain folder is not a repository");
    let branch = format!("wt-{tag}");
    let worktree = canvas_model::git::default_worktree_path(&probe.root, &branch);
    backend.worktree_add(&probe.root.display().to_string(), &worktree.display().to_string(), Some(&branch), "main").unwrap();
    let inside = backend.git_probe(&worktree.display().to_string()).unwrap().unwrap();
    assert_eq!(inside.branch.as_deref(), Some(branch.as_str()));
    assert!(backend.worktree_add(&probe.root.display().to_string(), &worktree.display().to_string(), Some("again"), "main").is_err(), "an occupied folder is refused");

    // Group folders, the event catalog and passive session files.
    let group = Group { id: "g".into(), name: "Team".into(), folder: Some(mine.join("group").display().to_string()), ..Default::default() };
    backend.prepare_group(&group).unwrap();
    assert!(mine.join("group").join("ai-docs").is_dir() && mine.join("group").join(".canvas").join("canvas-group.instructions.md").is_file());
    let catalog = backend.ensure_catalog(&mine.join("group").display().to_string()).unwrap();
    assert!(Path::new(&catalog).is_file());
    let files = PassiveFiles {
        folder: mine.join("group").display().to_string(),
        group_name: "Team".into(),
        base_slug: "guard".into(),
        taken: Vec::new(),
        name: "Guard".into(),
        rules: "# Rules\n".into(),
    };
    assert_eq!(backend.create_passive(files.clone()).unwrap(), "guard");
    assert_eq!(backend.create_passive(files).unwrap(), "guard-2", "an existing passive folder is never reused");
    let doctor = backend.doctor_folder().unwrap();
    assert_eq!(PathBuf::from(&doctor), ctx.home.join("factory"));

    // Automations from the sample data.db (no manifest folder).
    let load = backend.automations(None, false).unwrap();
    let mut keys: Vec<&str> = load.list.iter().map(|a| a.key.as_str()).collect();
    keys.sort();
    assert_eq!(keys, ["session:s1", "wf:w1", "wf:w2"]);
    assert!(load.errors.is_empty() && !load.factory_loaded);
    assert_eq!(PathBuf::from(&load.dir), ctx.home.join("automations"));
    answers.push(format!("{:?}", load.list.iter().map(|a| (&a.key, &a.name, &a.schedule, a.enabled, a.runs.len())).collect::<Vec<_>>()));
    let manifests = backend.automations_dir(Some(mine.display().to_string())).unwrap();
    assert_eq!(manifests, mine.display().to_string());
    backend.automation_set_enabled("wf:w2", true).unwrap();
    assert!(backend.automations(None, false).unwrap().list.iter().any(|a| a.key == "wf:w2" && a.enabled));
    backend.automation_set_enabled("wf:w2", false).unwrap();
    assert!(backend.automation_set_enabled("session:s1", true).is_err(), "session schedules are not toggled here");

    // Fork of a synthetic Copilot history.
    let parent = "11111111-1111-1111-1111-111111111111";
    let child = if tag == "local" { "22222222-2222-2222-2222-222222222222" } else { "33333333-3333-3333-3333-333333333333" };
    assert!(backend.has_history(AgentKind::Copilot, parent).unwrap());
    assert!(!backend.has_history(AgentKind::Copilot, child).unwrap());
    backend.fork(parent, child, "Forked", Some(&mine.display().to_string())).unwrap();
    assert!(backend.has_history(AgentKind::Copilot, child).unwrap());
    let yaml = std::fs::read_to_string(ctx.sessions_dir.join(child).join("workspace.yaml")).unwrap();
    assert!(yaml.contains(&format!("id: {child}")) && yaml.contains("name: \"Forked\""), "{yaml}");
    assert!(backend.fork(parent, child, "Again", None).is_err(), "never overwrites a session");

    // The read-only transcript of that history (what the inspector shows).
    let request = canvas_model::rpc::Request::Transcript { agent: AgentKind::Copilot, session: parent.into(), parent: None, agent_key: None, cwd: None, limit: 50 };
    let transcript = backend.transcript(request).unwrap();
    assert_eq!(transcript.entries.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(), ["hi"]);
    answers.push(serde_json::to_string(&transcript).unwrap());
    let bad = canvas_model::rpc::Request::Transcript { agent: AgentKind::Copilot, session: "not-a-session".into(), parent: None, agent_key: None, cwd: None, limit: 50 };
    answers.push(backend.transcript(bad).unwrap_err().to_string());

    // Canvas backups.
    let canvas = mine.join("canvas.json");
    let backup = backend.doc_backup(&canvas.display().to_string(), "before-test", Doc::default()).unwrap();
    assert!(Path::new(&backup).is_file());
    assert_eq!(backend.doc_backups(&canvas.display().to_string()).unwrap(), [backup]);

    // Failures come back as errors with the same text.
    let error = backend.open_target(&work.join("missing.txt").display().to_string()).unwrap_err().to_string();
    assert!(error.ends_with("does not exist"), "{error}");
    answers.push(error);
    answers
}

/// The shared fixture: folders, a Git repository and a Copilot session to fork.
fn fixture(service: &Service) -> PathBuf {
    let work = service.root.join("work");
    std::fs::create_dir_all(work.join("shared").join("Alpha")).unwrap();
    std::fs::create_dir_all(work.join("shared").join("beta")).unwrap();
    std::fs::write(work.join("shared").join("notes.txt"), "line1\nline2\nline3\n").unwrap();
    let repo = work.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["-c", "user.name=t", "-c", "user.email=t@example.invalid", "commit", "-q", "--allow-empty", "-m", "init"]);
    let parent = service.instance.sessions_dir.join("11111111-1111-1111-1111-111111111111");
    std::fs::create_dir_all(&parent).unwrap();
    std::fs::write(parent.join("workspace.yaml"), "id: 11111111-1111-1111-1111-111111111111\nname: Parent\n").unwrap();
    std::fs::write(
        parent.join("events.jsonl"),
        "{\"type\":\"session.start\",\"data\":{\"sessionId\":\"11111111-1111-1111-1111-111111111111\"},\"id\":\"e1\"}\n{\"type\":\"user.message\",\"data\":{\"content\":\"hi\"},\"id\":\"e2\"}\n",
    )
    .unwrap();
    work
}

#[test]
fn local_and_remote_backends_answer_the_same() {
    let service = Service::start("contract");
    let work = fixture(&service);
    let local = LocalBackend::new(service.ctx());
    let local_answers = contract(&local, &service.ctx(), &work, "local");
    let (conn, remote) = service.connect();
    let remote_answers = contract(&*remote, &service.ctx(), &work, "remote");
    assert_eq!(local_answers, remote_answers, "both backends give the same answers");
    // Unknown requests from a newer client fail cleanly; the connection stays.
    conn.tx.send(ClientMsg::Rpc { id: 999_999, json: r#"{"FromTheFuture":{}}"#.into() }).unwrap();
    let remote_dyn: &dyn Backend = &*remote;
    assert!(remote_dyn.info().is_ok(), "the connection survives an unknown request");
    // A lost connection fails calls instead of hanging.
    conn.tx.send(ClientMsg::Shutdown).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while remote_dyn.info().is_ok() {
        assert!(Instant::now() < deadline, "calls fail once the service is gone");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Needs an installed agent CLI (runs it): `cargo test -- --ignored config_snapshot`.
#[test]
#[ignore]
fn config_snapshot_is_the_same_locally_and_remotely() {
    let service = Service::start("config");
    let local = LocalBackend::new(service.ctx());
    let local_dyn: &dyn Backend = &local;
    let agent = local_dyn.info().unwrap().installed.first().copied().expect("an installed agent");
    let (_conn, remote) = service.connect();
    let remote_dyn: &dyn Backend = &*remote;
    let (a, b) = (local_dyn.config(agent).unwrap(), remote_dyn.config(agent).unwrap());
    assert_eq!(a, b);
    println!("{}: {} plugins, {} MCP servers, {} skills", agent.label(), a.plugins.len(), a.mcp.len(), a.skills.len());
}

/// A client of one document on a real service: its replica and working copy.
struct Window {
    conn: Conn,
    replica: Replica,
    doc: Doc,
    notices: Vec<String>,
}

impl Window {
    fn open(service: &Service, cwd: &str) -> Window {
        let (conn, _) = service.connect();
        let (opened, _) = conn::open_doc(&conn, None, true, cwd, Duration::from_secs(20)).unwrap();
        Window { conn, replica: Replica::new(&opened), doc: opened.doc, notices: Vec::new() }
    }

    fn send(&mut self) {
        if let Some((base, ops)) = self.replica.take_edit(&self.doc) {
            let ops_json = serde_json::to_string(&ops).unwrap();
            self.conn.tx.send(ClientMsg::DocEdit { key: self.replica.key.clone(), base, ops_json }).unwrap();
        }
    }

    /// Applies patches until nothing arrives for a moment.
    fn receive(&mut self) {
        while let Ok(message) = self.conn.rx.recv_timeout(Duration::from_millis(400)) {
            if let ServiceMsg::DocPatch { key, version, origin, ops_json } = message {
                assert_eq!(key, self.replica.key);
                let ops: Vec<Op> = serde_json::from_str(&ops_json).unwrap();
                let patched = self.replica.apply_patch(&mut self.doc, version, origin, &ops);
                assert!(!patched.resync, "no patch is missing");
                self.notices.extend(patched.overwritten);
            }
        }
    }
}

fn node(id: &str, x: i32) -> Node {
    serde_json::from_value(serde_json::json!({ "id": id, "kind": "session", "name": id, "x": x, "y": 30 })).unwrap()
}

#[test]
fn two_clients_editing_one_canvas_converge() {
    let service = Service::start("converge");
    let mut a = Window::open(&service, r"C:\");
    let mut b = Window::open(&service, r"C:\");
    assert_eq!(a.replica.key, b.replica.key);
    assert_ne!(a.replica.client, b.replica.client);
    // Both rename the same box, A also moves it; each adds one; A changes a
    // setting. A's edit reaches the service first; B's arrives before B saw A's patch.
    a.doc.node_mut("demo-main").unwrap().x += 6;
    a.doc.node_mut("demo-main").unwrap().name = "Renamed in A".into();
    a.doc.nodes.push(node("from-a", 0));
    a.doc.defaults.model = Some("model-a".into());
    b.doc.node_mut("demo-main").unwrap().name = "Renamed in B".into();
    b.doc.nodes.push(node("from-b", 40));
    b.doc.view.zoom = 2;
    a.send();
    a.receive();
    b.send();
    b.receive();
    a.receive();
    for window in [&a, &b] {
        assert_eq!(window.replica.in_flight(), 0);
    }
    assert!(sync::same_shared(&a.doc, &b.doc), "{:?}", sync::diff(&a.doc, &b.doc));
    let ids = |d: &Doc| d.nodes.iter().map(|n| n.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(&a.doc), ids(&b.doc), "same order");
    let main = a.doc.node("demo-main").unwrap();
    assert_eq!((main.name.as_str(), main.x), ("Renamed in B", 8), "the last edit wins per field; the move stays");
    assert_eq!(a.notices, ["“Renamed in A”"], "the overwritten side is told");
    assert!(b.notices.is_empty());
    assert_eq!(ids(&a.doc).iter().rev().take(2).collect::<Vec<_>>(), ["from-b", "from-a"]);
    assert_eq!(a.doc.defaults.model.as_deref(), Some("model-a"));
    assert_eq!((a.doc.view.zoom, b.doc.view.zoom), (0, 2), "views stay per client");
    // A third client sees the same document; the service saved it.
    let c = Window::open(&service, r"C:\");
    assert!(sync::same_shared(&c.doc, &a.doc));
    let path = service.instance.home.join("demo-canvas.json");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let saved: Option<Doc> = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok());
        if saved.as_ref().is_some_and(|s| sync::same_shared(s, &a.doc)) {
            break;
        }
        assert!(Instant::now() < deadline, "the service saves the document");
        std::thread::sleep(Duration::from_millis(50));
    }
    // An edit that would break the document is refused and the sender resynchronized.
    let duplicate = Op::PutGroup(Box::new(Group { id: "from-a".into(), name: "x".into(), w: 5, h: 5, ..Default::default() }));
    a.conn.tx.send(ClientMsg::DocEdit { key: a.replica.key.clone(), base: a.replica.version, ops_json: serde_json::to_string(&[duplicate]).unwrap() }).unwrap();
    let mut refused = (false, false);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !(refused.0 && refused.1) && Instant::now() < deadline {
        match a.conn.rx.recv_timeout(Duration::from_millis(200)) {
            Ok(ServiceMsg::Doc { .. }) => refused.0 = true,
            Ok(ServiceMsg::Error { message, .. }) if message.contains("refused") => refused.1 = true,
            _ => {}
        }
    }
    assert_eq!(refused, (true, true));
}
