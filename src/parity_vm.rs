//! Parity rows 57–62: VM sessions in the GUI (docs/VM-SESSIONS.md, Phase 4),
//! on an instance of their own whose service has the VM lane (`vm_lane`: a
//! pack whose provider is `vm::local`, sandboxes as local folders with their
//! satellites; the stand-in Copilot CLI in the guest). No cloud, no model.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{COLS, Gui, ROWS, Row, Sandbox, check, stop_service};
use crate::paths::Instance;
use crate::selftest::{GuiDriver, Ui};
use canvas_agents::vm::provider::Provider;
use crate::vm_lane::{Lane, REPO};

const ROWS_HERE: [&str; 6] = ["57", "58", "59", "60", "61", "62"];
const NAME: &str = "VM row box";

/// The rows' instance: its service with the lane, its canvas, its folder.
struct Vm {
    instance: Instance,
    lane: Lane,
    work: PathBuf,
    canvas: PathBuf,
    root: PathBuf,
    env: Vec<(&'static str, PathBuf)>,
    exe: PathBuf,
}

impl Vm {
    fn args(&self, extra: &[&str]) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "--instance".into(),
            self.instance.name.clone(),
            "--home".into(),
            self.instance.home.display().to_string(),
            "--sessions-dir".into(),
            self.instance.sessions_dir.display().to_string(),
            "--cwd".into(),
            self.work.display().to_string(),
            "--canvas".into(),
            self.canvas.display().to_string(),
        ];
        args.extend(extra.iter().map(|s| s.to_string()));
        args
    }

    fn start(&self, extra: &[&str], log: &str) -> Result<Gui> {
        let env: Vec<(&str, &Path)> = self.env.iter().map(|(k, v)| (*k, v.as_path())).collect();
        let gui = Gui { driver: GuiDriver::start(&self.args(extra), &env, (COLS, ROWS), &self.root.join(log))? };
        gui.wait("the VM canvas (☁ VMs in the top bar: the service has a VM pack)", 25, |t| t.contains("☁ VMs"))?;
        Ok(gui)
    }

    fn intent(&self) -> Result<canvas_agents::vm::launch::Intent> {
        crate::vm_lane::vm_box(&self.instance).context("the VM box's launch")
    }

    /// The satellites' last log lines (a failure's context).
    fn satellites(&self) -> String {
        let local = self.lane.local();
        let mut out = Vec::new();
        for sandbox in local.list() {
            if let Ok(bytes) = local.get_file(&sandbox.id, ".atelier/satellite.log", 1 << 20) {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let lines: Vec<&str> = text.lines().collect();
                out.push(format!("--- {} satellite.log ---\n{}", sandbox.id, lines[lines.len().saturating_sub(15)..].join("\n")));
            }
        }
        out.join("\n")
    }

    fn launches(&self, id: &str) -> usize {
        std::fs::read_to_string(self.instance.log_path()).unwrap_or_default().matches(&format!("launched {id} ")).count()
    }
}

pub(super) fn rows(sb: &Sandbox, rows: &mut Vec<Row>) {
    if let Ok(only) = std::env::var("CANVAS_PARITY_ROWS")
        && !only.split(',').any(|r| ROWS_HERE.contains(&r.trim()))
    {
        return;
    }
    let vm = match setup(sb) {
        Ok(Some(vm)) => vm,
        Ok(None) => {
            for row in ROWS_HERE {
                check(rows, row, "VM sessions", "vm_lane", || Ok("skipped: git is not installed (the VM lane clones a repository)".into()));
            }
            return;
        }
        Err(error) => {
            let error = format!("{error:#}");
            check(rows, "57", "VM sessions: the lane", "vm_lane", || bail!("{error}"));
            return;
        }
    };
    let mut gui: Option<Gui> = None;
    check(rows, "57", "*Run on: VM* in the new-session dialog", "vm_new_session_run_on", || {
        let started = vm.start(&[], "gui-vm.log")?;
        gui = Some(started);
        run_on(&vm, gui.as_mut().context("the GUI")?)
    });
    check(rows, "58", "☁ box states", "vm_box_states", || box_states(&vm, gui.as_mut().context("row 57 failed")?).map_err(|e| anyhow::anyhow!("{e:#}\n{}", vm.satellites())));
    check(rows, "61", "☁ VMs panel", "vm_panel", || panel(gui.as_mut().context("row 57 failed")?));
    check(rows, "60", "Tasks: Start on VM", "vm_tasks_start_on_vm", || tasks(&vm, gui.as_mut().context("row 57 failed")?));
    check(rows, "62", "VM boxes in remote mode", "vm_remote_loopback", || {
        if let Some(local) = gui.take() {
            quit(local);
        }
        let remote = vm.start(&["--remote", "loopback"], "gui-vm-remote.log")?;
        gui = Some(remote);
        remote_mode(&vm, gui.as_mut().context("the GUI")?)
    });
    check(rows, "59", "VM details and actions", "vm_details_actions", || details(&vm, gui.as_mut().context("row 57 failed")?));
    if let Some(gui) = gui {
        quit(gui);
    }
    vm.lane.clean();
    stop_service(&vm.instance);
}

fn setup(sb: &Sandbox) -> Result<Option<Vm>> {
    let root = sb.root.join("vm");
    let Some(lane) = Lane::new(&root, "vm-work")? else { return Ok(None) };
    let instance = Instance::new(Some(format!("parityvm{}", std::process::id())), Some(root.join("home")), Some(root.join("sessions")))?;
    std::fs::create_dir_all(&instance.sessions_dir)?;
    let (work, copilot_home) = (root.join("vm-work"), root.join("copilot-home"));
    std::fs::create_dir_all(&work)?;
    std::fs::create_dir_all(&copilot_home)?;
    let canvas = instance.home.join("vm-canvas.json");
    std::fs::write(&canvas, serde_json::to_string_pretty(&crate::client::doc::Doc::default())?)?;
    let env: Vec<(&'static str, PathBuf)> = vec![
        ("COPILOT_HOME", copilot_home),
        ("ATELIER_COPILOT", sb.fake.clone()),
        (crate::fake_copilot::ENV, PathBuf::from("1")),
        ("ATELIER_SELFTEST", PathBuf::from("1")),
    ];
    let service_env: Vec<(String, String)> = env.iter().map(|(k, v)| ((*k).to_owned(), v.display().to_string())).collect();
    lane.start_service(&sb.exe, &instance, &service_env)?;
    Ok(Some(Vm { instance, lane, work, canvas, root, env, exe: sb.exe.clone() }))
}

fn quit(gui: Gui) {
    let _ = gui.req(json!({ "cmd": "quit" }));
    let mut gui = gui;
    let _ = gui.driver.wait_exit(Duration::from_secs(10));
}

fn click_text(gui: &Gui, needle: &str) -> Result<()> {
    let at = gui.find(needle).with_context(|| format!("{needle:?} on screen\n{}", gui.screen()))?;
    gui.click(at, &[])
}

/// The VM box in the `tree` hook (`vm`: its VM's state word).
fn vm_box(gui: &Gui) -> Option<Value> {
    gui.tree().ok()?["canvas"]["boxes"].as_array()?.iter().find(|b| b["name"] == NAME).cloned()
}

fn wait_vm(gui: &Gui, what: &str, secs: u64, state: &str) -> Result<()> {
    gui.wait_tree(what, secs, |t| t["canvas"]["boxes"].as_array().is_some_and(|b| b.iter().any(|b| b["name"] == NAME && b["vm"] == state))).map(|_| ())
}

/// Row 57: New session › Run on: ☁ VM › Create & start: a ☁ box that starts
/// in its VM (its states on the way) and shows the agent from the guest.
fn run_on(vm: &Vm, gui: &mut Gui) -> Result<String> {
    gui.canvas_focus()?;
    gui.send("n")?;
    gui.wait("the new-session dialog with Run on (this machine by default)", 5, |t| t.contains("Run on") && t.contains("◉ This machine") && t.contains("○ ☁ VM"))?;
    gui.send(NAME)?;
    let at = gui.find("○ ☁ VM").context("Run on's ☁ VM")?;
    gui.click((at.0 + 2.0, at.1), &[])?;
    gui.wait("☁ VM chosen, the repository found from the folder", 5, |t| t.contains("◉ ☁ VM") && t.contains(REPO) && t.contains("atelier/<the session's name>"))?;
    click_text(gui, "Create & start")?;
    // Its VM's states on the way, as the tree hook tells them.
    let mut seen: Vec<String> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(b) = vm_box(gui)
            && let Some(state) = b["vm"].as_str()
            && seen.last().map(String::as_str) != Some(state)
        {
            anyhow::ensure!(b["prefix"] == "☁ ", "the box is marked ☁: {b}");
            seen.push(state.to_owned());
        }
        if gui.screen().contains("Fake Copilot") {
            break;
        }
        if Instant::now() > deadline {
            bail!("the agent did not start in its VM (states seen: {seen:?})\n{}", gui.screen());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    wait_vm(gui, "the box's VM running", 10, "running")?;
    let intent = vm.intent()?;
    let doc = crate::selftest::load(&vm.canvas)?;
    anyhow::ensure!(doc.node(&intent.r#box).is_some_and(|n| n.placement.as_ref().is_some_and(|p| p.is_vm())), "the canvas file has its placement");
    anyhow::ensure!(std::fs::read_to_string(vm.instance.log_path()).unwrap_or_default().contains(&format!("launched {} on a VM", intent.r#box)), "the service started it on a VM");
    Ok(format!("Run on ☁ VM ({REPO} found from the folder); the ☁ box went {} and shows the agent from its VM {}", seen.join(" → "), intent.sandbox))
}

/// Row 58: suspended (drawn and in the tree); typing into it resumes it and the keys arrive.
fn box_states(vm: &Vm, gui: &mut Gui) -> Result<String> {
    let id = vm.intent()?.r#box;
    let (ok, out) = vm.lane.cli(&vm.exe, &vm.instance, &["vm", "suspend", &id])?;
    anyhow::ensure!(ok, "vm suspend: {out}");
    wait_vm(gui, "suspended in the tree", 30, "suspended")?;
    let b = vm_box(gui).context("the box")?;
    anyhow::ensure!(b["line"].as_str().is_some_and(|l| l.contains("☁ suspended")), "its status line: {b}");
    gui.open(NAME, "Fake Copilot")?;
    gui.send("-typed-to-resume")?;
    gui.wait_tile("the keys typed into the suspended VM", 30, |t| t.contains("-typed-to-resume"))?;
    wait_vm(gui, "running again", 30, "running")?;
    anyhow::ensure!(std::fs::read_to_string(vm.instance.log_path()).unwrap_or_default().contains("typing resumes its VM"), "typing resumed it");
    Ok("suspended: “☁ suspended” and vm=suspended; typing resumed it and the keys arrived".into())
}

/// Row 61: the toolbar's ☁ VMs: the budget, the box, the image.
fn panel(gui: &mut Gui) -> Result<String> {
    gui.canvas_focus()?;
    click_text(gui, "☁ VMs")?;
    gui.wait("the ☁ VMs panel", 10, |t| t.contains("VM BOXES") && t.contains(&format!("{NAME} · running")) && t.contains("IMAGES") && t.contains(&format!("{REPO} · built")) && t.contains("Running    1 of 2 VMs"))?;
    Ok("budget (1 of 2 VMs), the box running, its repository's image built, Build now".into())
}

/// Row 60: a todo's *Start on VM…*: the dialog on a VM with the todo's
/// branch and a first prompt that ends with a pull request; nothing starts.
fn tasks(vm: &Vm, gui: &mut Gui) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let (ok, out) = vm.lane.cli(&vm.exe, &vm.instance, &["todo", "board", "add", "VM board", "--prefix", "VMB"])?;
        if ok {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "todo board add: {out}");
        std::thread::sleep(Duration::from_millis(300));
    }
    let (ok, out) = vm.lane.cli(&vm.exe, &vm.instance, &["todo", "add", "Run it on a VM", "--type", "feature", "--board", "VMB", "--status", "todo"])?;
    anyhow::ensure!(ok, "todo add: {out}");
    let before = std::fs::read_to_string(vm.instance.log_path()).unwrap_or_default().matches("launched ").count();
    gui.canvas_focus()?;
    gui.chord('3', &["ctrl"])?;
    gui.wait("the board in Tasks", 15, |t| t.contains("VMB-001"))?;
    click_text(gui, "Run it on a VM")?;
    gui.wait("its details with Start on VM…", 10, |t| t.contains("Start on VM…"))?;
    click_text(gui, "Start on VM…")?;
    gui.wait("the dialog on a VM, the todo's branch, a first prompt that ends with a pull request", 10, |t| {
        t.contains("◉ ☁ VM") && t.contains("atelier/VMB-001-run-it-on-a-vm") && t.contains("--pr URL")
    })?;
    gui.key("Esc", &[])?;
    gui.chord('1', &["ctrl"])?;
    std::thread::sleep(Duration::from_millis(500));
    anyhow::ensure!(std::fs::read_to_string(vm.instance.log_path()).unwrap_or_default().matches("launched ").count() == before, "nothing starts before Create");
    Ok("VMB-001's Start on VM…: ◉ ☁ VM, atelier/VMB-001-run-it-on-a-vm, a first prompt ending with todo submit --pr; nothing started".into())
}

/// Row 62: over `--remote loopback`: typing into the VM box, then Suspend
/// and Resume from its details (everything through the service).
fn remote_mode(vm: &Vm, gui: &mut Gui) -> Result<String> {
    let log = std::fs::read_to_string(vm.instance.log_path()).unwrap_or_default();
    anyhow::ensure!(log.contains("connected (gui, protocol 8"), "a remote GUI");
    gui.open(NAME, "Fake Copilot")?;
    gui.send("-over-tcp")?;
    gui.wait_tile("typing over TCP reaches the guest", 20, |t| t.contains("-over-tcp"))?;
    let id = vm.intent()?.r#box;
    let launches = vm.launches(&id);
    gui.canvas_focus()?;
    click_text(gui, "☁ VMs")?;
    gui.wait("the ☁ VMs panel", 10, |t| t.contains(&format!("{NAME} · running")))?;
    click_text(gui, &format!("{NAME} · running"))?;
    gui.wait("its details", 10, |t| t.contains("Suspend VM"))?;
    click_text(gui, "Suspend VM")?;
    wait_vm(gui, "suspended from its details", 30, "suspended")?;
    gui.wait("Resume VM", 10, |t| t.contains("Resume VM"))?;
    click_text(gui, "Resume VM")?;
    wait_vm(gui, "resumed from its details", 30, "running")?;
    anyhow::ensure!(vm.launches(&id) == launches, "viewing its details started it again");
    Ok("over loopback TCP: typed into the guest; Suspend VM and Resume VM from its details".into())
}

/// Row 59: the box menu's *VM details…*, *Update from main* (a message for
/// the agent), *Delete VM…* stopped by the barrier (an untracked file in the
/// guest), then *Delete anyway*: deleted, its credential revoked.
fn details(vm: &Vm, gui: &mut Gui) -> Result<String> {
    let intent = vm.intent()?;
    gui.canvas_focus()?;
    if gui.box_center(NAME).is_err() {
        gui.send("f")?;
    }
    let at = gui.box_center(NAME)?;
    gui.mouse("down", "right", at, &[])?;
    gui.mouse("up", "right", at, &[])?;
    gui.wait("the box menu with VM details…", 5, |t| t.contains("☁ VM details…") && t.contains("Suspend VM"))?;
    click_text(gui, "☁ VM details…")?;
    gui.wait("its details", 10, |t| t.contains("Update from main") && t.contains("Delete VM…") && t.contains("Link       linked"))?;
    click_text(gui, "Update from main")?;
    gui.wait("the message asked of the agent", 10, |t| t.contains("Asked the agent to bring in origin/main"))?;
    vm.lane.local().put_file(&intent.sandbox, &format!("work/{REPO}/not-pushed.txt"), b"work\n", 0o644)?;
    click_text(gui, "Delete VM…")?;
    gui.wait("asked first", 5, |t| t.contains(&format!("Delete the VM of “{NAME}”?")))?;
    click_text(gui, "Delete VM")?;
    gui.wait("what the barrier found", 30, |t| t.contains("anyway?") && t.contains("1 new file not in git"))?;
    click_text(gui, "Delete anyway")?;
    gui.wait_tree("its VM deleted", 60, |t| t["canvas"]["boxes"].as_array().is_some_and(|b| b.iter().any(|b| b["name"] == NAME && (b["vm"] == "deleting" || b["vm"] == "deleted"))))?;
    let deadline = Instant::now() + Duration::from_secs(60);
    while vm.lane.local().get(&intent.sandbox).is_ok() {
        anyhow::ensure!(Instant::now() < deadline, "its sandbox was not deleted");
        std::thread::sleep(Duration::from_millis(200));
    }
    let revoked = matches!(canvas_agents::vm::store::Satellites::new(&vm.instance.home).credential(&intent.satellite), canvas_common::handshake::Credential::Revoked(_));
    anyhow::ensure!(revoked, "its satellite credential is revoked");
    Ok("menu › VM details…; Update from main asked the agent; Delete VM… stopped at “1 new file not in git”; Delete anyway: sandbox gone, credential revoked".into())
}
