//! The self-test's VM step (docs/VM-SESSIONS.md; Phase 4), on a canvas of
//! its own with the VM lane (`vm_lane`: a pack whose provider is
//! `vm::local`, sandboxes as local folders): a box created with *Run on:
//! ☁ VM* starts in its VM, takes typing, shows working then idle; its
//! details (from the ☁ VMs panel) suspend and resume it; viewing starts
//! nothing. The same in the TUI and the GUI, also over `--remote loopback`.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use super::{Ui, step};
use crate::paths::Instance;
use crate::vm_lane::Lane;

pub(super) fn vm_steps<U: Ui>(instance: &Instance, args: &[String], env: &[(&str, &Path)], start: &impl Fn(&[String], &[(&str, &Path)]) -> Result<U>) -> Result<()> {
    step("a box Run on ☁ VM (local provider): it starts in its VM, takes typing, shows working then idle; Suspend and Resume from its details; viewing starts nothing");
    let root = instance.home.join("vm-lane");
    let Some(lane) = Lane::new(&root, "vm-work")? else {
        println!("    (skipped: git is not installed)");
        return Ok(());
    };
    let result = run(&lane, instance, &root, args, env, start);
    // Whatever happened: no sandbox's process outlives the step, nor the lane's service.
    lane.clean();
    crate::parity::stop_service(instance);
    result
}

fn run<U: Ui>(lane: &Lane, instance: &Instance, root: &Path, base: &[String], env: &[(&str, &Path)], start: &impl Fn(&[String], &[(&str, &Path)]) -> Result<U>) -> Result<()> {
    let exe = std::env::current_exe()?;
    let service_env: Vec<(String, String)> = env.iter().map(|(k, v)| ((*k).to_owned(), v.display().to_string())).collect();
    lane.start_service(&exe, instance, &service_env)?;
    let work = root.join("vm-work");
    std::fs::create_dir_all(&work)?;
    let canvas = instance.home.join("vm-canvas.json");
    std::fs::write(&canvas, serde_json::to_string_pretty(&crate::client::doc::Doc::default())?)?;
    // This canvas, its folder the lane's repository's: no --demo.
    let mut args: Vec<String> = Vec::new();
    let mut words = base.iter().filter(|a| *a != "--demo").cloned();
    while let Some(word) = words.next() {
        if word == "--cwd" {
            words.next();
            continue;
        }
        args.push(word);
    }
    args.extend(["--canvas".into(), canvas.display().to_string(), "--cwd".into(), work.display().to_string()]);
    let mut ui = start(&args, env)?;
    ui.wait("the VM canvas (☁ VMs in the top bar: the service has a VM pack)", Duration::from_secs(20), |t| t.contains("☁ VMs"))?;

    // New session › Run on: ☁ VM › Create & start.
    ui.send(b"\x1d")?;
    ui.send(b"n")?;
    ui.wait("the new-session dialog with Run on", Duration::from_secs(5), |t| t.contains("Run on") && t.contains("○ ☁ VM"))?;
    ui.send(b"VM box")?;
    let (x, y) = ui.find("○ ☁ VM").context("Run on's ☁ VM")?;
    ui.click(x + 2, y)?;
    ui.wait("☁ VM chosen, the repository found from the folder", Duration::from_secs(5), |t| t.contains("◉ ☁ VM") && t.contains(crate::vm_lane::REPO))?;
    // (Enter would act on the focused Run on row.)
    let (x, y) = ui.find("Create & start").context("Create & start")?;
    ui.click(x + 1, y)?;
    ui.wait("the agent in its VM (an image built, the VM claimed, its satellite linked)", Duration::from_secs(120), |t| t.contains("Fake Copilot"))?;
    let intent = crate::vm_lane::vm_box(instance).context("the VM box's launch")?;
    let id = intent.r#box.clone();
    let doc = super::load(&canvas)?;
    anyhow::ensure!(doc.node(&id).is_some_and(|n| n.placement.as_ref().is_some_and(|p| p.is_vm())), "the canvas file has the box's placement");
    anyhow::ensure!(std::fs::read_to_string(instance.log_path()).unwrap_or_default().contains(&format!("launched {id} on a VM")), "the service started it on a VM");

    // Typing reaches the agent in the guest; its status follows the guest's history.
    ui.send(b"hello-vm")?;
    ui.wait("the typing back from the guest", Duration::from_secs(15), |t| t.contains("hello-vm"))?;
    ui.send(b"\r")?;
    ui.wait("working", Duration::from_secs(20), |t| t.contains("VM box · working"))?;
    ui.wait("then idle", Duration::from_secs(30), |t| t.contains("VM box · idle"))?;

    // Its details from the ☁ VMs panel: Suspend, then Resume. Viewing starts nothing.
    let launches = || std::fs::read_to_string(instance.log_path()).unwrap_or_default().matches(&format!("launched {id} ")).count();
    let before = launches();
    ui.send(b"\x1d")?;
    let (x, y) = ui.find("☁ VMs").context("the top bar's ☁ VMs")?;
    ui.click(x + 1, y)?;
    ui.wait("the ☁ VMs panel", Duration::from_secs(5), |t| t.contains("VM BOXES") && t.contains("VM box · running"))?;
    let (x, y) = ui.find("VM box · running").context("the box in the panel")?;
    ui.click(x + 1, y)?;
    ui.wait("its details with Suspend VM", Duration::from_secs(5), |t| t.contains("☁ VM") && t.contains("Suspend VM"))?;
    let (x, y) = ui.find("Suspend VM").context("Suspend VM")?;
    ui.click(x + 1, y)?;
    ui.wait("suspended", Duration::from_secs(30), |t| t.contains("Resume VM") && t.contains("suspended"))?;
    anyhow::ensure!(crate::vm_lane::vm_box(instance).is_some_and(|i| i.state == canvas_model::vm::VmState::Suspended), "the VM is suspended");
    let (x, y) = ui.find("Resume VM").context("Resume VM")?;
    ui.click(x + 1, y)?;
    ui.wait("running again", Duration::from_secs(30), |t| t.contains("Suspend VM"))?;
    anyhow::ensure!(crate::vm_lane::vm_box(instance).is_some_and(|i| i.state == canvas_model::vm::VmState::Running), "the VM runs again");
    anyhow::ensure!(launches() == before, "viewing the panel and the details started the session again");

    ui.send(b"\x1d")?;
    ui.send(b"q")?;
    std::thread::sleep(Duration::from_millis(300));
    if ui.text().contains("Detach (keep running)") {
        ui.send(b"\r")?;
    }
    let code = ui.wait_exit(Duration::from_secs(10))?;
    anyhow::ensure!(code == 0, "client exited with {code}");
    // Its VM deleted (the guest's work is the lane's: lost on purpose).
    let (ok, out) = lane.cli(&exe, instance, &["vm", "delete", &id, "--yes", "--discard"])?;
    anyhow::ensure!(ok, "vm delete: {out}");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !lane.local().list().is_empty() {
        anyhow::ensure!(std::time::Instant::now() < deadline, "the VM's sandbox was not deleted");
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}
