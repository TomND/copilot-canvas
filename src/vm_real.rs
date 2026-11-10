//! `atelier selftest vm --real` (VM-51): the owner's check of VM sessions on
//! the real provider of the configured pack, through the running service:
//! the pack, the provider's sign-in, an image, a VM box with the real agent
//! CLI started without a prompt (nothing reaches a model), typing, a second
//! client's replay, suspend and resume, the delete barrier and the delete.
//! It prints step names, outcomes and durations only: never a value of the
//! pack (details go to `<home>/selftest-vm-real.log`), and every VM it made
//! is deleted whatever happens. `--json FILE` writes the report.
//!
//! `--dry-run` runs the same scenario on an instance of its own with the VM
//! lane (`vm::local`, the stand-in agent): `selftest vm` runs it, so CI does.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::client::app::TermView;
use crate::client::conn::{self, Conn};
use crate::paths::Instance;
use crate::protocol::{ClientMsg, Input, LaunchKind, ServiceMsg};

/// How long a VM may take to be claimed and started on a real provider.
const CLAIM_SECS: u64 = 20 * 60;

struct Step {
    name: &'static str,
    ok: bool,
    ms: u64,
    note: String,
}

/// What the run made, deleted at its end whatever happened.
struct Made {
    exe: PathBuf,
    instance: Instance,
    env: Vec<(String, String)>,
    boxes: Vec<String>,
    canvas: Option<PathBuf>,
}

impl Made {
    fn cli(&self, args: &[&str]) -> Result<(bool, String)> {
        crate::vm_probe::cli(&self.exe, args, &self.env)
    }
}

impl Drop for Made {
    fn drop(&mut self) {
        let home = self.instance.home.clone();
        for id in &self.boxes {
            let _ = self.cli(&["vm", "delete", id, "--yes", "--discard"]);
        }
        // Waits for the service to delete them; then asks the provider itself.
        let deadline = Instant::now() + Duration::from_secs(300);
        let sandboxes = || self.boxes.iter().filter_map(|id| canvas_agents::vm::launch::load(&home, id).ok().flatten()).filter(|i| !i.sandbox.is_empty() && i.state != canvas_model::vm::VmState::Deleted).map(|i| i.sandbox).collect::<Vec<_>>();
        while !sandboxes().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_secs(2));
        }
        let left = sandboxes();
        if !left.is_empty()
            && let Some((pack, _)) = canvas_agents::vm::loaded(&home)
            && let Ok(provider) = canvas_agents::vm::open_provider(&home, &pack)
        {
            for sandbox in &left {
                let _ = provider.delete(sandbox);
            }
        }
        if let Some(canvas) = &self.canvas {
            let _ = std::fs::remove_file(canvas);
        }
        println!("  cleanup: {} VM box(es) made by this run deleted{}", self.boxes.len(), if left.is_empty() { String::new() } else { format!(" ({} through the provider directly)", left.len()) });
    }
}

pub fn run(instance: &Instance, dry: bool, agent: &str, json_out: Option<&Path>) -> Result<()> {
    let exe = std::env::current_exe()?;
    if dry {
        let root = std::env::temp_dir().join(format!("atelier-vm-real-dry-{}", uuid::Uuid::new_v4()));
        let dry_instance = Instance::new(Some(format!("vmrealdry{}", std::process::id())), Some(root.join("home")), Some(root.join("sessions")))?;
        std::fs::create_dir_all(&dry_instance.sessions_dir)?;
        let Some(lane) = crate::vm_lane::Lane::new(&root, "real-work")? else {
            println!("  · skipped: git is not installed");
            return Ok(());
        };
        let fake = root.join("fake-cli");
        std::fs::create_dir_all(&fake)?;
        for name in ["copilot", "claude"] {
            let path = fake.join(canvas_sys::host::exe_name(name));
            if std::fs::hard_link(&exe, &path).is_err() || cfg!(windows) {
                let _ = std::fs::remove_file(&path);
                std::fs::copy(&exe, &path)?;
            }
        }
        let copilot_home = root.join("copilot-home");
        std::fs::create_dir_all(&copilot_home)?;
        let env: Vec<(String, String)> = vec![
            ("ATELIER_COPILOT".into(), fake.join(canvas_sys::host::exe_name("copilot")).display().to_string()),
            ("ATELIER_CLAUDE".into(), fake.join(canvas_sys::host::exe_name("claude")).display().to_string()),
            (crate::fake_copilot::ENV.into(), "1".into()),
            ("COPILOT_HOME".into(), copilot_home.display().to_string()),
            ("ATELIER_SELFTEST".into(), "1".into()),
        ];
        // The lane's pack is this instance's (settings.json), as a real one is.
        canvas_common::settings::Settings { vm: canvas_common::settings::VmSettings { pack: Some(lane.pack.display().to_string()) }, ..Default::default() }.save(&dry_instance.home)?;
        lane.start_service(&exe, &dry_instance, &env)?;
        let mut cli_env = vec![("ATELIER_VM_PACK".to_owned(), lane.pack.display().to_string())];
        cli_env.extend(base_env(&dry_instance));
        let result = scenario(&exe, &dry_instance, cli_env, true, agent, &root.join("real-work"), json_out);
        lane.clean();
        crate::parity::stop_service(&dry_instance);
        if result.is_ok() {
            let _ = std::fs::remove_dir_all(&root);
        }
        return result;
    }
    let work = instance.home.join("selftest-vm-real-work");
    std::fs::create_dir_all(&work)?;
    scenario(&exe, instance, base_env(instance), false, agent, &work, json_out)
}

fn base_env(instance: &Instance) -> Vec<(String, String)> {
    vec![("ATELIER_HOME".into(), instance.home.display().to_string()), (crate::vm_probe::PROBE_INSTANCE.into(), instance.name.clone())]
}

fn screen(view: &TermView) -> String {
    view.lines.iter().map(|row| row.iter().map(|r| r.text.as_str()).collect::<String>()).collect::<Vec<_>>().join("\n")
}

/// Follows box `id`'s frames until `done` (or the time is up).
fn pump(conn: &Conn, view: &mut TermView, id: &str, secs: u64, done: impl Fn(&TermView) -> bool) -> bool {
    let until = Instant::now() + Duration::from_secs(secs);
    loop {
        if done(view) {
            return true;
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        match conn.rx.recv_timeout(left.min(Duration::from_millis(200))) {
            Ok(ServiceMsg::Frame8(frame)) if frame.id == id => view.apply8(frame),
            Ok(ServiceMsg::Error { message, .. }) => {
                let _ = std::fs::OpenOptions::new().create(true).append(true).open(errors_file()).and_then(|mut f| std::io::Write::write_all(&mut f, format!("{message}\n").as_bytes()));
            }
            _ => {}
        }
    }
}

fn wait_intent(home: &Path, id: &str, secs: u64, ok: impl Fn(&canvas_agents::vm::launch::Intent) -> bool) -> Result<canvas_agents::vm::launch::Intent> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(intent) = canvas_agents::vm::launch::load(home, id)?
            && ok(&intent)
        {
            return Ok(intent);
        }
        if Instant::now() > deadline {
            bail!("timed out");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[allow(clippy::too_many_arguments)]
fn scenario(exe: &Path, instance: &Instance, env: Vec<(String, String)>, dry: bool, agent: &str, work: &Path, json_out: Option<&Path>) -> Result<()> {
    use canvas_model::vm::VmState;
    println!("Atelier VM sessions on the pack's provider{} (the {agent} CLI, never a prompt; no pack values are printed)", if dry { ", dry run on local sandboxes" } else { "" });
    let home = instance.home.clone();
    let log_path = home.join("selftest-vm-real.log");
    let mut log = std::fs::File::create(&log_path)?;
    let mut steps: Vec<Step> = Vec::new();
    let mut made = Made { exe: exe.to_path_buf(), instance: instance.clone(), env, boxes: Vec::new(), canvas: None };
    let mut step = |name: &'static str, result: Result<String>, started: Instant, log: &mut std::fs::File| -> bool {
        use std::io::Write;
        let ms = started.elapsed().as_millis() as u64;
        let ok = result.is_ok();
        let note = match result {
            Ok(note) => note,
            Err(error) => {
                let _ = writeln!(log, "{name}: {error:#}");
                // The dry run's errors name nothing of a real pack.
                if dry { format!("{error:#}") } else { format!("failed (details in {})", log_path.display()) }
            }
        };
        println!("  {} {name} ({:.1} s){}", if ok { "✓" } else { "✗" }, ms as f64 / 1000.0, if note.is_empty() { String::new() } else { format!(": {note}") });
        steps.push(Step { name, ok, ms, note });
        ok
    };
    let result = (|| -> Result<()> {
        let t = Instant::now();
        let pack = canvas_agents::vm::loaded(&home);
        let checked = match &pack {
            Some((pack, status)) if status.errors() == 0 => Ok(format!("{} repositor(ies), {} size(s)", pack.repos.len(), pack.sizes.len())),
            Some((_, status)) => Err(anyhow::anyhow!("the pack has {} error(s): atelier vm pack check", status.errors())),
            None => Err(anyhow::anyhow!("no VM pack is configured")),
        };
        if !step("the pack is usable", checked, t, &mut log) {
            bail!("the pack");
        }
        let (pack, _) = pack.context("checked")?;
        let t = Instant::now();
        if !step("the provider signs in", canvas_agents::vm::open_provider(&home, &pack).map(|_| String::new()), t, &mut log) {
            bail!("the provider");
        }
        let t = Instant::now();
        let conn = conn::try_connect(instance).context("the Atelier service is not running for this instance: start Atelier first");
        if !step("the service runs", conn.as_ref().map(|_| String::new()).map_err(|e| anyhow::anyhow!("{e:#}")), t, &mut log) {
            bail!("the service");
        }
        let conn = conn?;
        let repo = pack.repos.first().context("the pack has no repository")?.key.clone();

        // An image of the repository (the newest, else one built now).
        let t = Instant::now();
        let ready = |home: &Path| canvas_agents::vm::store::load_images(home).map(|f| f.images.iter().any(|i| i.repo == repo && i.state == "ready")).unwrap_or(false);
        let image = if ready(&home) {
            Ok("the newest".to_owned())
        } else {
            let _ = made.cli(&["vm", "image", "build", &repo]);
            let deadline = Instant::now() + Duration::from_secs(CLAIM_SECS);
            while !ready(&home) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_secs(2));
            }
            if ready(&home) { Ok("built now".to_owned()) } else { Err(anyhow::anyhow!("no image was built in time")) }
        };
        if !step("an image of the repository", image, t, &mut log) {
            bail!("the image");
        }

        // A VM box on a canvas of its own, its agent started without a prompt.
        let t = Instant::now();
        let id = uuid::Uuid::new_v4().to_string();
        let canvas = home.join(format!("selftest-vm-real-{}.json", &id[..8]));
        made.canvas = Some(canvas.clone());
        conn.tx.send(ClientMsg::DocOpen { path: Some(canvas.display().to_string()), demo: false, cwd: work.display().to_string() })?;
        let opened = wait_doc(&conn)?;
        let mut node: canvas_model::doc::Node = serde_json::from_value(json!({ "id": id, "kind": "session", "name": "VM self-test", "x": 0, "y": 0, "agent": agent, "fresh": true }))?;
        node.placement = Some(canvas_model::vm::Placement::Vm(Box::new(canvas_model::vm::VmPlacement { repo: repo.clone(), branch: Some(format!("atelier/selftest-{}", &id[..8])), ..Default::default() })));
        node.cwd = Some(work.display().to_string());
        conn.tx.send(ClientMsg::DocEdit { key: opened.0, base: opened.1, ops_json: serde_json::to_string(&vec![canvas_model::sync::Op::PutNode(Box::new(node.clone()))])? })?;
        let routing = json!({ "canvas": "", "groups": [], "members": {}, "passive": [], "no_messages": [], "names": { id.clone(): "VM self-test" }, "agents": { id.clone(): agent } });
        conn.tx.send(ClientMsg::Routing { json: routing.to_string() })?;
        let launch = canvas_model::launch::agent_launch(&node, &Default::default(), None, None, None, false);
        made.boxes.push(id.clone());
        conn.tx.send(ClientMsg::Subscribe8 { ids: vec![id.clone()], focused: Some(id.clone()) })?;
        conn.tx.send(ClientMsg::Launch { id: id.clone(), kind: LaunchKind::Agent(Box::new(launch)), cwd: work.display().to_string(), cols: 100, rows: 30 })?;
        let mut view = TermView::default();
        let running = wait_intent(&home, &id, CLAIM_SECS, |i| i.state == VmState::Running || matches!(i.state, VmState::Failed | VmState::Lost));
        let started = running.as_ref().is_ok_and(|i| i.state == VmState::Running) && pump(&conn, &mut view, &id, 300, |v| screen(v).trim().len() > 20);
        let outcome = if started { Ok(String::new()) } else { Err(anyhow::anyhow!("the box did not start ({:?}); screen:\n{}", running.as_ref().map(|i| (i.state, i.error.clone())), screen(&view))) };
        if !step("a VM box starts and shows its agent from the guest", outcome, t, &mut log) {
            bail!("the box");
        }
        let intent = canvas_agents::vm::launch::load(&home, &id)?.context("its launch")?;

        // Typing reaches the guest (no Enter: nothing goes to a model), once its program runs there.
        let t = Instant::now();
        let deadline = Instant::now() + Duration::from_secs(300);
        let accepted = loop {
            let _ = std::fs::remove_file(errors_file());
            conn.tx.send(ClientMsg::Input { id: id.clone(), input: Input::Raw(b"q".to_vec()) })?;
            pump(&conn, &mut view, &id, 2, |_| false);
            let refused = std::fs::read_to_string(errors_file()).unwrap_or_default();
            if !refused.contains("input not sent") {
                break true;
            }
            if Instant::now() > deadline {
                break false;
            }
        };
        let echoed = accepted && pump(&conn, &mut view, &id, 20, |v| screen(v).contains('q'));
        conn.tx.send(ClientMsg::Input { id: id.clone(), input: Input::Raw(vec![0x7f]) })?;
        let typed = if !accepted { Err(anyhow::anyhow!("its program never took input")) } else { Ok(if echoed { "echoed".into() } else { "sent (its prompt did not echo it)".into() }) };
        step("typing reaches the agent in the guest", typed, t, &mut log);

        // A second client gets the screen again (the hub's replay).
        let t = Instant::now();
        let second = conn::try_connect(instance)?;
        second.tx.send(ClientMsg::Subscribe8 { ids: vec![id.clone()], focused: None })?;
        let mut replay = TermView::default();
        let replayed = pump(&second, &mut replay, &id, 30, |v| screen(v).trim().len() > 20);
        drop(second);
        step("a second client gets the screen again (replay)", if replayed { Ok(String::new()) } else { Err(anyhow::anyhow!("no screen")) }, t, &mut log);

        // Suspend, then resume.
        let t = Instant::now();
        let suspended = made.cli(&["vm", "suspend", &id]).and_then(|(ok, out)| if ok { wait_intent(&home, &id, 600, |i| i.state == VmState::Suspended).map(|_| String::new()) } else { bail!("{out}") });
        step("suspend", suspended, t, &mut log);
        let t = Instant::now();
        let resumed = made.cli(&["vm", "resume", &id]).and_then(|(ok, out)| if ok { wait_intent(&home, &id, 600, |i| i.state == VmState::Running).map(|_| String::new()) } else { bail!("{out}") });
        // Its satellite linked again, steadily (it may dial twice as it wakes).
        let linked = || canvas_agents::vm::link::load_record(&home, &intent.sandbox).is_some_and(|r| r.linked);
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut steady = 0;
        while steady < 6 && Instant::now() < deadline {
            steady = if linked() { steady + 1 } else { 0 };
            std::thread::sleep(Duration::from_millis(500));
        }
        let resumed = resumed.and_then(|note| if steady >= 6 { Ok(note) } else { bail!("its satellite did not link again") });
        step("resume (its satellite linked again)", resumed, t, &mut log);

        // The delete barrier: a new file in the box's repository, then the delete.
        let t = Instant::now();
        let provider = canvas_agents::vm::open_provider(&home, &pack)?;
        let barrier = provider
            .put_file(&intent.sandbox, &format!("work/{repo}/atelier-selftest-untracked.txt"), b"made by atelier selftest vm --real\n", 0o644)
            .and_then(|()| {
                let (ok, out) = made.cli(&["vm", "delete", &id, "--yes"])?;
                if dry && !(!ok && out.contains("not in git")) {
                    let tail = provider.get_file(&intent.sandbox, ".atelier/satellite.log", 1 << 20).map(|b| String::from_utf8_lossy(&b).lines().rev().take(25).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")).unwrap_or_default();
                    bail!("the barrier did not stop the delete (its workspace {}): {out}\n--- satellite.log ---\n{tail}", intent.cwd);
                }
                anyhow::ensure!(!ok && out.contains("not in git"), "the barrier did not stop the delete: {out}");
                Ok(String::new())
            });
        step("the delete barrier names a new file not in git", barrier, t, &mut log);
        let t = Instant::now();
        let deleted = made.cli(&["vm", "delete", &id, "--yes", "--discard"]).and_then(|(ok, out)| {
            anyhow::ensure!(ok, "{out}");
            wait_intent(&home, &id, 600, |i| i.state == VmState::Deleted)?;
            let gone = || matches!(canvas_agents::vm::store::Satellites::new(&home).credential(&intent.satellite), canvas_common::handshake::Credential::Revoked(_)) && matches!(provider.get(&intent.sandbox), Ok(None));
            let deadline = Instant::now() + Duration::from_secs(if dry { 20 } else { 300 });
            while !gone() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_secs(1));
            }
            let revoked = matches!(canvas_agents::vm::store::Satellites::new(&home).credential(&intent.satellite), canvas_common::handshake::Credential::Revoked(_));
            anyhow::ensure!(gone(), "{} is still there", if revoked { "the sandbox" } else { "its satellite credential" });
            Ok(String::new())
        });
        step("delete: the sandbox goes, its credential is revoked", deleted, t, &mut log);
        conn.tx.send(ClientMsg::Stop { id: id.clone(), force: true })?;
        Ok(())
    })();
    drop(made);
    let failed = steps.iter().filter(|s| !s.ok).count();
    let report = json!({
        "ok": result.is_ok() && failed == 0,
        "dry_run": dry,
        "agent": agent,
        "steps": steps.iter().map(|s| json!({ "step": s.name, "ok": s.ok, "ms": s.ms, "note": s.note })).collect::<Vec<_>>(),
    });
    if let Some(path) = json_out {
        std::fs::write(path, serde_json::to_string_pretty(&report)?)?;
    }
    match result {
        Ok(()) if failed == 0 => {
            println!("PASS: {} step(s)", steps.len());
            Ok(())
        }
        _ => bail!("{} step(s) failed (details in {})", failed.max(1), log_path.display()),
    }
}

/// The service's errors while a step pumps (this run's own file).
fn errors_file() -> PathBuf {
    std::env::temp_dir().join(format!("atelier-vm-real-errors-{}.log", std::process::id()))
}

/// The canvas the connection opened: (key, version).
fn wait_doc(conn: &Conn) -> Result<(String, u64)> {
    let until = Instant::now() + Duration::from_secs(20);
    while Instant::now() < until {
        if let Ok(ServiceMsg::Doc { key, version, .. }) = conn.rx.recv_timeout(Duration::from_millis(200)) {
            return Ok((key, version));
        }
    }
    bail!("the canvas did not open")
}
