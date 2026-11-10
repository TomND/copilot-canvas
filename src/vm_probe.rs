//! `atelier selftest vm`: VM sessions end to end on this machine, with no
//! cloud (docs/VM-SESSIONS.md §11). A service in a temporary home uses a
//! VM pack whose hooks are `atelier vm-hook-standin` (sandboxes as local
//! folders: `vm::local`) and the stand-in Copilot CLI, which keeps a Copilot
//! history in its guest. Checked: the pack, a box that runs on a VM (its
//! terminal shows the agent from the guest, its status follows the guest's
//! history through the hub's mirror), canvas commands run in the guest and
//! applied on the hub as that box (a todo claimed, a report), commands of
//! the hub's machine refused in the guest, and a box placed on a VM never
//! started here by a launch that does not say so.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::client::app::TermView;
use crate::client::conn::{self, Conn};
use crate::paths::Instance;
use crate::protocol::{ClientMsg, Input, LaunchKind, ServiceMsg};

const BOX: &str = "0b1e5ca1-0000-4000-8000-00000000c10d";
const PLACED: &str = "0b1e5ca1-0000-4000-8000-0000000b10cc";
const CANVAS: &str = "c0ffee00-0000-4000-8000-00000000ca5e";
const SECOND: &str = "0b1e5ca1-0000-4000-8000-00000000c20d";

fn step(text: &str) {
    println!("  · {text}");
}

pub fn run(exe: &Path) -> Result<()> {
    let root = std::env::temp_dir().join(format!("atelier-vm-probe-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("sessions"))?;
    let instance = Instance::new(Some(format!("vmprobe{}", std::process::id())), Some(root.join("home")), Some(root.join("sessions")))?;
    let started = Instant::now();
    let result = scenario(exe, &instance, &root);
    if let Ok(conn) = conn::try_connect(&instance) {
        let _ = conn.tx.send(ClientMsg::Shutdown);
        std::thread::sleep(Duration::from_millis(1500));
    }
    match &result {
        Ok(()) => {
            println!("PASS in {:.1} s", started.elapsed().as_secs_f32());
            for _ in 0..10 {
                if std::fs::remove_dir_all(&root).is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        }
        Err(_) => eprintln!("artifacts kept in {} (service log: {})", root.display(), instance.log_path().display()),
    }
    result
}

/// A hard link of this program under another name (a copy where links are not possible).
fn alias(exe: &Path, path: &Path) -> Result<()> {
    if std::fs::hard_link(exe, path).is_err() {
        std::fs::copy(exe, path)?;
    }
    Ok(())
}

/// A hook of the probe's pack: `atelier vm-hook-standin NAME`.
fn hook(dir: &Path, exe: &Path, name: &str) -> Result<()> {
    if cfg!(windows) {
        std::fs::write(dir.join(format!("{name}.cmd")), format!("@\"{}\" vm-hook-standin {name}\r\n", exe.display()))?;
    } else {
        std::fs::write(dir.join(format!("{name}.sh")), format!("exec \"{}\" vm-hook-standin {name}\n", exe.display()))?;
    }
    Ok(())
}

/// A local repository to clone into sandboxes (`None` without git).
fn bare_repo(root: &Path) -> Option<PathBuf> {
    let bare = root.join("repo.git");
    let seed = root.join("seed");
    let git = |args: &[&str], cwd: &Path| Command::new("git").args(args).current_dir(cwd).env("GIT_TERMINAL_PROMPT", "0").output().is_ok_and(|o| o.status.success());
    std::fs::create_dir_all(&seed).ok()?;
    let ok = git(&["init", "--quiet", "--bare", "--initial-branch=main", &bare.display().to_string()], root)
        && git(&["init", "--quiet", "--initial-branch=main"], &seed)
        && std::fs::write(seed.join("README.md"), "probe\n").is_ok()
        && git(&["-c", "user.name=Probe", "-c", "user.email=probe@example.com", "add", "README.md"], &seed)
        && git(&["-c", "user.name=Probe", "-c", "user.email=probe@example.com", "commit", "--quiet", "-m", "seed"], &seed)
        && git(&["push", "--quiet", &bare.display().to_string(), "main"], &seed);
    ok.then_some(bare)
}

fn write_pack(root: &Path, exe: &Path) -> Result<PathBuf> {
    let pack = root.join("pack");
    std::fs::create_dir_all(pack.join("hooks"))?;
    std::fs::create_dir_all(pack.join("egress"))?;
    let url = bare_repo(root).map(|p| p.display().to_string()).unwrap_or_else(|| "https://example.com/contoso/app.git".into());
    let local = root.join("sandboxes");
    std::fs::write(
        pack.join("vm.yaml"),
        format!("pack_version: 1\nname: probe\nprovider: hooks\npolicy_ack: \"2026-11-09\"\npoll_seconds: 1\nbudget:\n  max_running: 1\nsizes:\n  - name: S\n    cpu: 1000m\n    memory: 2048Mi\n    disk: 20Gi\nlocal:\n  root: {}\n", serde_json::to_string(&local.display().to_string())?),
    )?;
    std::fs::write(pack.join("repos.yaml"), format!("repos:\n  probe-app:\n    url: {}\n    default_branch: main\n    egress: probe\n    folders: [\"**/work\"]\n", serde_json::to_string(&url)?))?;
    std::fs::write(pack.join("egress").join("probe.yaml"), "default: deny\nrules:\n  - host: example.com\n    action: allow\n")?;
    for name in ["claim", "shell-command", "exec", "suspend", "resume", "delete"] {
        hook(&pack.join("hooks"), exe, name)?;
    }
    Ok(pack)
}

/// The terminal and service messages the probe follows.
struct Watch {
    conn: Conn,
    view: TermView,
    second: TermView,
    /// The activities the box's sessions summary went through.
    activities: Vec<crate::protocol::Activity>,
    errors: Vec<String>,
    doc: Option<(String, u64)>,
}

impl Watch {
    fn pump(&mut self, until: Instant, mut done: impl FnMut(&Watch) -> bool) -> bool {
        loop {
            if done(self) {
                return true;
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            match self.conn.rx.recv_timeout(left.min(Duration::from_millis(100))) {
                Ok(ServiceMsg::Frame8(frame)) if frame.id == BOX => self.view.apply8(frame),
                Ok(ServiceMsg::Frame8(frame)) if frame.id == SECOND => self.second.apply8(frame),
                Ok(ServiceMsg::Sessions { upsert, .. }) => {
                    for s in upsert.into_iter().filter(|s| s.id == BOX) {
                        if self.activities.last() != Some(&s.activity) {
                            self.activities.push(s.activity);
                        }
                    }
                }
                Ok(ServiceMsg::Error { message, .. }) => self.errors.push(message),
                Ok(ServiceMsg::Doc { key, version, .. }) => self.doc = Some((key, version)),
                Ok(ServiceMsg::DocPatch { key, version, .. }) => self.doc = Some((key, version)),
                _ => {}
            }
        }
    }

    fn screen(&self) -> String {
        self.view.lines.iter().map(|row| row.iter().map(|r| r.text.as_str()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }
}

/// Runs this program as a command of the guest (relayed) or of the hub.
fn cli(exe: &Path, args: &[&str], env: &[(String, String)]) -> Result<(bool, String)> {
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(std::process::Stdio::null());
    for key in ["ATELIER_SESSION", "ATELIER_REMOTE", "ATELIER_PASSIVE", "AUTOMATION_NAME"] {
        cmd.env_remove(key);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output()?;
    Ok((out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))))
}

fn wait_file(what: &str, secs: u64, mut check: impl FnMut() -> bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !check() {
        anyhow::ensure!(Instant::now() < deadline, "{what} (waited {secs} s)");
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

fn scenario(exe: &Path, instance: &Instance, root: &Path) -> Result<()> {
    println!("Atelier VM sessions self-test (the thin lane on local sandboxes; no cloud, no model)");
    let pack = write_pack(root, exe)?;
    let fake = root.join("fake-cli");
    std::fs::create_dir_all(&fake)?;
    let copilot = fake.join(canvas_sys::host::exe_name("copilot"));
    alias(exe, &copilot)?;
    let work = root.join("work");
    std::fs::create_dir_all(&work)?;
    let home = instance.home.display().to_string();
    let hub_env: Vec<(String, String)> = vec![("ATELIER_HOME".into(), home.clone()), ("ATELIER_VM_PACK".into(), pack.display().to_string())];

    step("atelier vm pack check: the pack is usable");
    let (ok, out) = cli(exe, &["vm", "pack", "check"], &hub_env)?;
    anyhow::ensure!(ok && out.contains("The pack is usable."), "pack check: {out}");

    step("the spikes' kit on local sandboxes: the agent CLI in a guest (s2), this build in the guest (s3), claim latency (s4)");
    let spike_env: Vec<(String, String)> = hub_env.iter().cloned().chain([("ATELIER_COPILOT".to_owned(), copilot.display().to_string()), (crate::fake_copilot::ENV.to_owned(), "1".to_owned())]).collect();
    for spike in [&["vm", "spike", "s2", "--agent", "copilot"][..], &["vm", "spike", "s3"], &["vm", "spike", "s4", "--runs", "2"]] {
        let (ok, out) = cli(exe, spike, &spike_env)?;
        let value: serde_json::Value = serde_json::from_str(out.trim()).with_context(|| format!("{spike:?} printed no JSON: {out}"))?;
        anyhow::ensure!(ok && value["ok"] == true, "{spike:?}: {out}");
        anyhow::ensure!(!out.contains(&pack.display().to_string()), "a spike printed the pack's folder");
    }
    anyhow::ensure!(canvas_agents::vm::local::Local::new(root.join("sandboxes")).list().is_empty(), "the spikes left sandboxes behind");

    let copilot_home = root.join("hub-copilot");
    std::fs::create_dir_all(&copilot_home)?;
    let env = [
        ("ATELIER_VM_PACK", pack.display().to_string()),
        ("ATELIER_COPILOT", copilot.display().to_string()),
        (crate::fake_copilot::ENV, "1".into()),
        ("ATELIER_FAKE_HISTORY", "1".into()),
        ("ATELIER_FAKE_TURN_MS", "3000".into()),
        ("COPILOT_HOME", copilot_home.display().to_string()),
        ("ATELIER_SELFTEST", "1".into()),
    ];
    let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    conn::spawn_service_exe(exe, instance, &env_refs)?;
    let deadline = Instant::now() + Duration::from_secs(20);
    let conn = loop {
        match conn::try_connect(instance) {
            Ok(conn) => break conn,
            Err(error) if Instant::now() > deadline => bail!("the service did not start: {error:#}"),
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    };
    let mut watch = Watch { conn, view: TermView::default(), second: TermView::default(), activities: Vec::new(), errors: Vec::new(), doc: None };
    let routing = serde_json::json!({
        "canvas": CANVAS, "groups": [], "members": {}, "passive": [], "no_messages": [],
        "names": { BOX: "VM box", PLACED: "Placed box" }, "agents": { BOX: "copilot", PLACED: "copilot" },
    });
    watch.conn.tx.send(ClientMsg::Routing { json: routing.to_string() })?;

    step("a box placed on a VM in the canvas is never started on this machine by a launch that does not say so");
    let vm = canvas_model::vm::Placement::Vm(Box::new(canvas_model::vm::VmPlacement { repo: "probe-app".into(), branch: Some("atelier/probe".into()), ..Default::default() }));
    let node = |id: &str, name: &str| -> Result<canvas_model::doc::Node> {
        let mut node: canvas_model::doc::Node = serde_json::from_value(serde_json::json!({ "id": id, "kind": "session", "name": name, "x": 0, "y": 0, "agent": "copilot", "fresh": true }))?;
        node.placement = Some(vm.clone());
        node.cwd = Some(work.display().to_string());
        Ok(node)
    };
    watch.conn.tx.send(ClientMsg::DocOpen { path: Some(instance.home.join("vm-probe.json").display().to_string()), demo: false, cwd: work.display().to_string() })?;
    anyhow::ensure!(watch.pump(Instant::now() + Duration::from_secs(10), |w| w.doc.is_some()), "the canvas did not open");
    let (key, version) = watch.doc.clone().context("doc")?;
    let ops = vec![canvas_model::sync::Op::PutNode(Box::new(node(PLACED, "Placed box")?))];
    watch.conn.tx.send(ClientMsg::DocEdit { key, base: version, ops_json: serde_json::to_string(&ops)? })?;
    anyhow::ensure!(watch.pump(Instant::now() + Duration::from_secs(10), |w| w.doc.as_ref().is_some_and(|(_, v)| *v > version)), "the canvas edit was not applied");
    let plain = canvas_common::protocol::AgentLaunch { agent: "copilot".into(), ..Default::default() };
    watch.conn.tx.send(ClientMsg::Launch { id: PLACED.into(), kind: LaunchKind::Agent(Box::new(plain)), cwd: work.display().to_string(), cols: 100, rows: 30 })?;
    anyhow::ensure!(
        watch.pump(Instant::now() + Duration::from_secs(10), |w| w.errors.iter().any(|e| e.contains("runs on a VM"))),
        "an old-style launch of a VM box was not refused: {:?}",
        watch.errors
    );
    let log = std::fs::read_to_string(instance.log_path()).unwrap_or_default();
    anyhow::ensure!(!log.contains(&format!("launched {PLACED}")), "the placed box was started here");

    step("a box that runs on a VM: the claim, then the agent in the guest in its terminal");
    let launch = canvas_model::launch::agent_launch(&node(BOX, "VM box")?, &Default::default(), None, None, None, false);
    anyhow::ensure!(launch.agent == "vm:copilot", "the launch names a VM agent: {}", launch.agent);
    watch.conn.tx.send(ClientMsg::Subscribe8 { ids: vec![BOX.into()], focused: Some(BOX.into()) })?;
    watch.conn.tx.send(ClientMsg::Launch { id: BOX.into(), kind: LaunchKind::Agent(Box::new(launch)), cwd: work.display().to_string(), cols: 100, rows: 30 })?;
    anyhow::ensure!(
        watch.pump(Instant::now() + Duration::from_secs(30), |w| w.screen().contains("Fake Copilot")),
        "the agent did not start in the guest; errors {:?}; screen:\n{}",
        watch.errors,
        watch.screen()
    );
    let intent = canvas_agents::vm::launch::load(&instance.home, BOX)?.context("the launch's intent")?;
    anyhow::ensure!(!intent.sandbox.is_empty() && intent.state == canvas_model::vm::VmState::Running, "the intent: {intent:?}");
    let local = canvas_agents::vm::local::Local::new(root.join("sandboxes"));
    let sandbox = local.get(&intent.sandbox)?;
    anyhow::ensure!(sandbox.branch == "atelier/probe" && Path::new(&sandbox.cwd).is_dir(), "the sandbox: {sandbox:?}");

    step("typing reaches the agent in the guest; its status follows the guest's history (working, then idle)");
    watch.conn.tx.send(ClientMsg::Input { id: BOX.into(), input: Input::Raw(b"hello from the hub\r".to_vec()) })?;
    anyhow::ensure!(
        watch.pump(Instant::now() + Duration::from_secs(30), |w| w.activities.contains(&crate::protocol::Activity::Working) && w.activities.last() == Some(&crate::protocol::Activity::Idle)),
        "the box's status did not follow the guest (seen {:?}); screen:\n{}",
        watch.activities,
        watch.screen()
    );
    let mirror = canvas_agents::vm::thin::mirror_dir(&instance.home, &intent.sandbox).join("copilot").join(BOX).join("events.jsonl");
    anyhow::ensure!(std::fs::read_to_string(&mirror).unwrap_or_default().contains("user.message"), "the hub's mirror has the turn");

    step("canvas commands in the guest are applied on this machine as the box (a todo claimed, a report)");
    let (ok, out) = cli(exe, &["todo", "board", "add", "Probe", "--prefix", "PRB"], &hub_env)?;
    anyhow::ensure!(ok, "todo board add: {out}");
    let (ok, out) = cli(exe, &["todo", "add", "Probe task", "--type", "chore", "--status", "todo", "--board", "Probe"], &hub_env)?;
    anyhow::ensure!(ok, "todo add: {out}");
    let mut guest = local.env(&intent.sandbox)?;
    guest.extend([("ATELIER_REMOTE".into(), "satellite-lite".into()), ("ATELIER_SESSION".into(), BOX.into())]);
    let (ok, out) = cli(exe, &["todo", "claim", "PRB-001"], &guest)?;
    anyhow::ensure!(ok, "todo claim from the guest: {out}");
    let book = instance.home.join("canvas-agents").join(CANVAS).join("todos").join("book.json");
    wait_file("the todo was not claimed by the box", 30, || std::fs::read_to_string(&book).is_ok_and(|t| t.contains(BOX) && t.contains("in_progress")))?;
    let (ok, out) = cli(exe, &["report", "publish", "--message", "Written in the VM"], &guest)?;
    anyhow::ensure!(ok, "report publish from the guest: {out}");
    let items = instance.home.join("reports").join("items");
    wait_file("the report did not reach the inbox as the box", 30, || {
        std::fs::read_dir(&items).is_ok_and(|e| e.flatten().any(|i| std::fs::read_to_string(i.path().join("meta.json")).is_ok_and(|m| m.contains(BOX) && m.contains("Written in the VM"))))
    })?;

    step("the satellite credential is minted for the VM and written in the guest");
    let credential = Path::new(&sandbox.cwd).ancestors().nth(2).context("the sandbox folder")?.join("home").join(".atelier").join("satellite.json");
    let text = std::fs::read_to_string(&credential).with_context(|| format!("no credential at {}", credential.display()))?;
    anyhow::ensure!(text.contains(&intent_now(instance)?.satellite) && text.contains("\"secret\""), "the guest's credential: {}", credential.display());

    step("a budget of one running VM queues a second VM box and says why; suspending the first starts it");
    let second = node(SECOND, "Second VM box")?;
    let launch = canvas_model::launch::agent_launch(&second, &Default::default(), None, None, None, false);
    watch.conn.tx.send(ClientMsg::Subscribe8 { ids: vec![BOX.into(), SECOND.into()], focused: Some(SECOND.into()) })?;
    watch.conn.tx.send(ClientMsg::Launch { id: SECOND.into(), kind: LaunchKind::Agent(Box::new(launch)), cwd: work.display().to_string(), cols: 100, rows: 30 })?;
    let second_screen = |w: &Watch| w.second.lines.iter().map(|row| row.iter().map(|r| r.text.as_str()).collect::<String>()).collect::<Vec<_>>().join("\n");
    anyhow::ensure!(
        watch.pump(Instant::now() + Duration::from_secs(20), |w| second_screen(w).contains("Waiting to start: 1 of 1 VMs are running")),
        "the second box is not queued:\n{}",
        second_screen(&watch)
    );
    let (ok, out) = cli(exe, &["vm", "list"], &hub_env)?;
    anyhow::ensure!(ok && out.contains(&format!("{SECOND}  queued")), "vm list: {out}");
    let (ok, out) = cli(exe, &["vm", "suspend", BOX], &hub_env)?;
    anyhow::ensure!(ok && out.contains("Suspending"), "vm suspend: {out}");
    anyhow::ensure!(
        watch.pump(Instant::now() + Duration::from_secs(30), |w| second_screen(w).contains("Fake Copilot")),
        "the second box did not start once the first was suspended:\n{}",
        second_screen(&watch)
    );
    anyhow::ensure!(local.get(&intent.sandbox)?.suspended, "the stand-in's sandbox is suspended");
    let (ok, out) = cli(exe, &["vm", "budget"], &hub_env)?;
    anyhow::ensure!(ok && out.contains("VMs running 1 of 1"), "vm budget: {out}");

    step("deleting a VM: refused without --yes; its sandbox goes and its satellite credential is revoked");
    let (ok, _) = cli(exe, &["vm", "delete", SECOND], &hub_env)?;
    anyhow::ensure!(!ok, "a delete without --yes");
    let second_intent = canvas_agents::vm::launch::load(&instance.home, SECOND)?.context("the second intent")?;
    let (ok, out) = cli(exe, &["vm", "delete", SECOND, "--yes"], &hub_env)?;
    anyhow::ensure!(ok, "vm delete: {out}");
    let revoked = || matches!(canvas_agents::vm::store::Satellites::new(&instance.home).credential(&second_intent.satellite), canvas_common::handshake::Credential::Revoked(_));
    wait_file("the second VM was not deleted and its credential revoked", 20, || local.get(&second_intent.sandbox).is_err() && revoked())?;
    let left: Vec<String> = local.list().into_iter().map(|s| s.id).collect();
    anyhow::ensure!(left == [intent.sandbox.clone()], "no sandbox leaked: {left:?}");
    watch.conn.tx.send(ClientMsg::Stop { id: SECOND.into(), force: true })?;
    let (ok, out) = cli(exe, &["vm", "resume", BOX], &hub_env)?;
    anyhow::ensure!(ok, "vm resume: {out}");
    wait_file("the first VM did not resume", 20, || local.get(&intent.sandbox).is_ok_and(|s| !s.suspended))?;
    let (_, out) = cli(exe, &["doctor"], &hub_env)?;
    anyhow::ensure!(out.contains("VM pack") && out.contains("VM sessions"), "doctor has the VM lines: {out}");

    step("commands of this machine are refused in the guest");
    let (ok, out) = cli(exe, &["service", "stop"], &guest)?;
    anyhow::ensure!(!ok && out.contains("not available inside a VM session"), "service stop in the guest: {out}");
    let (ok, out) = cli(exe, &["note", "path"], &guest)?;
    anyhow::ensure!(!ok && out.contains("note new"), "note path in the guest: {out}");

    step("the box stops like any session");
    watch.conn.tx.send(ClientMsg::Stop { id: BOX.into(), force: false })?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let log = std::fs::read_to_string(instance.log_path()).unwrap_or_default();
        if log.contains(&format!("terminal {BOX} exited")) {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "the box did not stop");
        watch.pump(Instant::now() + Duration::from_millis(200), |_| false);
    }
    Ok(())
}

/// The first box's intent as it is now.
fn intent_now(instance: &Instance) -> Result<canvas_agents::vm::launch::Intent> {
    canvas_agents::vm::launch::load(&instance.home, BOX)?.context("the launch's intent")
}
