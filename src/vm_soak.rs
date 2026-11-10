//! `atelier selftest vm --soak [--minutes N]` (VM-52): a VM box on the VM
//! lane (`vm::local`, the stand-in agent) through faults for N minutes (20
//! by default): freezes (suspend and resume), restarts of the service while
//! the guest works, and typing into a suspended VM. Every cycle types a
//! numbered line (one turn of the stand-in) and runs a canvas command from
//! the guest. Checked at the end: every line typed is in the mirrored
//! history exactly once, every command applied exactly once; reported: the
//! hub's replays and gaps, and the service's and the satellite's memory at
//! the start and the end.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::client::conn;
use crate::paths::Instance;
use crate::protocol::{ClientMsg, Input, LaunchKind};

pub fn run(minutes: u64) -> Result<()> {
    let exe = std::env::current_exe()?;
    let root = std::env::temp_dir().join(format!("atelier-vm-soak-{}", uuid::Uuid::new_v4()));
    let instance = Instance::new(Some(format!("vmsoak{}", std::process::id())), Some(root.join("home")), Some(root.join("sessions")))?;
    std::fs::create_dir_all(&instance.sessions_dir)?;
    let Some(lane) = crate::vm_lane::Lane::new(&root, "soak-work")? else { bail!("the soak needs git (its VM lane clones a repository)") };
    let result = soak(&exe, &instance, &lane, &root, minutes);
    lane.clean();
    crate::parity::stop_service(&instance);
    match &result {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&root);
        }
        Err(_) => eprintln!("artifacts kept in {}", root.display()),
    }
    result
}

fn rss(pid: Option<u32>) -> u64 {
    pid.map(|p| canvas_sys::process::process_usage(Some(p)).working_set / (1 << 20)).unwrap_or(0)
}

fn service_pid(home: &Path) -> Option<u32> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(home.join("reports").join("service.json")).ok()?).ok()?;
    v["pid"].as_u64().map(|p| p as u32)
}

/// The soak box's satellite: the newest `atelier satellite` started since
/// `since_ms` (an image's builder had one before it).
fn satellite_pid(since_ms: u64) -> Option<u32> {
    canvas_sys::process::process_entries()
        .into_iter()
        .map(|(pid, _, _)| pid)
        .filter_map(|pid| canvas_sys::process::process_start_ms(pid).filter(|s| *s >= since_ms).map(|s| (s, pid)))
        .filter(|(_, pid)| canvas_sys::process::process_argv(*pid).is_some_and(|argv| argv.last().is_some_and(|a| a == "satellite")))
        .max()
        .map(|(_, pid)| pid)
}

fn soak(exe: &Path, instance: &Instance, lane: &crate::vm_lane::Lane, root: &Path, minutes: u64) -> Result<()> {
    use canvas_model::vm::VmState;
    println!("Atelier VM soak: {minutes} min on local sandboxes (freezes, service restarts, typing into a suspended VM; the stand-in agent)");
    let soak_ms = crate::paths::now_ms();
    let home = instance.home.clone();
    canvas_common::settings::Settings { vm: canvas_common::settings::VmSettings { pack: Some(lane.pack.display().to_string()) }, ..Default::default() }.save(&home)?;
    let fake = root.join("fake-cli").join(canvas_sys::host::exe_name("copilot"));
    std::fs::create_dir_all(fake.parent().context("fake")?)?;
    if cfg!(windows) || std::fs::hard_link(exe, &fake).is_err() {
        std::fs::copy(exe, &fake)?;
    }
    let copilot_home = root.join("copilot-home");
    std::fs::create_dir_all(&copilot_home)?;
    let env: Vec<(String, String)> = vec![
        ("ATELIER_COPILOT".into(), fake.display().to_string()),
        (crate::fake_copilot::ENV.into(), "1".into()),
        ("COPILOT_HOME".into(), copilot_home.display().to_string()),
        ("ATELIER_SELFTEST".into(), "1".into()),
        ("ATELIER_FAKE_TURN_MS".into(), "50".into()),
    ];
    lane.start_service(exe, instance, &env)?;
    let hub = vec![("ATELIER_HOME".to_owned(), home.display().to_string()), (crate::vm_probe::PROBE_INSTANCE.to_owned(), instance.name.clone())];
    let work = root.join("soak-work");
    std::fs::create_dir_all(&work)?;

    // The box.
    let id = uuid::Uuid::new_v4().to_string();
    let mut conn = conn::try_connect(instance)?;
    let mut node: canvas_model::doc::Node = serde_json::from_value(json!({ "id": id, "kind": "session", "name": "Soak box", "x": 0, "y": 0, "agent": "copilot", "fresh": true }))?;
    node.placement = Some(canvas_model::vm::Placement::Vm(Box::new(canvas_model::vm::VmPlacement { repo: crate::vm_lane::REPO.into(), branch: Some("atelier/soak".into()), ..Default::default() })));
    node.cwd = Some(work.display().to_string());
    let launch = canvas_model::launch::agent_launch(&node, &Default::default(), None, None, None, false);
    conn.tx.send(ClientMsg::Launch { id: id.clone(), kind: LaunchKind::Agent(Box::new(launch)), cwd: work.display().to_string(), cols: 100, rows: 30 })?;
    let intent = wait(60, || canvas_agents::vm::launch::load(&home, &id).ok().flatten().filter(|i| i.state == VmState::Running)).context("the box did not start")?;
    let mut guest = lane.local().env(&intent.sandbox)?;
    guest.extend([("ATELIER_REMOTE".into(), "satellite-lite".into()), ("ATELIER_SESSION".into(), id.clone())]);
    // Typing is taken once its program runs in the guest.
    wait(120, || {
        let linked = canvas_agents::vm::link::load_record(&home, &intent.sandbox).is_some_and(|r| r.linked);
        (linked && mirrored_turns(&home, &intent.sandbox, &id).is_some()).then_some(())
    })
    .context("the agent's history never reached the mirror")?;

    let started = Instant::now();
    let until = started + Duration::from_secs(minutes * 60);
    let (hub_rss0, sat_rss0) = (rss(service_pid(&home)), rss(satellite_pid(soak_ms)));
    let (mut lines, mut ops, mut cycle) = (0u64, 0u64, 0u64);
    let mut faults = [0u32; 4];
    let typed = |conn: &conn::Conn, n: u64| conn.tx.send(ClientMsg::Input { id: id.clone(), input: Input::Raw(format!("soak-{n}\r").into_bytes()) });
    while Instant::now() < until {
        cycle += 1;
        let fault = (cycle % 4) as usize;
        faults[fault] += 1;
        match fault {
            // A freeze: suspended and resumed.
            1 => {
                let (ok, out) = crate::vm_probe::cli(exe, &["vm", "suspend", &id], &hub)?;
                anyhow::ensure!(ok, "vm suspend: {out}");
                wait(60, || canvas_agents::vm::launch::load(&home, &id).ok().flatten().filter(|i| i.state == VmState::Suspended)).context("not suspended")?;
                std::thread::sleep(Duration::from_millis(500));
                let (ok, out) = crate::vm_probe::cli(exe, &["vm", "resume", &id], &hub)?;
                anyhow::ensure!(ok, "vm resume: {out}");
            }
            // The service restarts while the guest works; a command meanwhile is queued in the guest.
            2 => {
                drop(conn);
                crate::parity::stop_service(instance);
                ops += 1;
                let (ok, out) = crate::vm_probe::cli(exe, &["report", "publish", "--message", &format!("soak-op-{ops}")], &guest)?;
                anyhow::ensure!(ok, "a command while the service is away: {out}");
                lane.start_service(exe, instance, &env)?;
                conn = conn::try_connect(instance)?;
            }
            // Typing into a suspended VM resumes it (and the keys arrive once).
            3 => {
                let (ok, out) = crate::vm_probe::cli(exe, &["vm", "suspend", &id], &hub)?;
                anyhow::ensure!(ok, "vm suspend: {out}");
                wait(60, || canvas_agents::vm::launch::load(&home, &id).ok().flatten().filter(|i| i.state == VmState::Suspended)).context("not suspended")?;
            }
            _ => {}
        }
        // The VM back and linked (faults 1 and 2), or resumed by this typing (3).
        if fault != 3 {
            wait(120, || canvas_agents::vm::link::load_record(&home, &intent.sandbox).filter(|r| r.linked).map(|_| ())).with_context(|| format!("cycle {cycle}: the satellite did not link again"))?;
        }
        lines += 1;
        let mut sent = false;
        for _ in 0..100 {
            typed(&conn, lines)?;
            // Refused while its program is not reachable yet: typed again (a refusal sends nothing).
            std::thread::sleep(Duration::from_millis(300));
            let errors: Vec<String> = conn.rx.try_iter().filter_map(|m| if let crate::protocol::ServiceMsg::Error { message, .. } = m { Some(message) } else { None }).collect();
            if !errors.iter().any(|e| e.contains("input not sent")) {
                sent = true;
                break;
            }
        }
        anyhow::ensure!(sent, "cycle {cycle}: typing was refused for 30 s");
        ops += 1;
        let (ok, out) = crate::vm_probe::cli(exe, &["report", "publish", "--message", &format!("soak-op-{ops}")], &guest)?;
        anyhow::ensure!(ok, "cycle {cycle}: a command from the guest: {out}");
        if cycle % 10 == 0 {
            println!("  · {} min: {cycle} cycles, {lines} lines typed, {ops} commands", started.elapsed().as_secs() / 60);
        }
    }
    let (hub_rss1, sat_rss1) = (rss(service_pid(&home)), rss(satellite_pid(soak_ms)));

    // Exactly once: every line in the mirrored history, every command applied.
    let turns = wait(120, || mirrored_turns(&home, &intent.sandbox, &id).filter(|n| *n >= lines)).unwrap_or_else(|| mirrored_turns(&home, &intent.sandbox, &id).unwrap_or(0));
    let reports = |n: u64| -> usize {
        std::fs::read_dir(home.join("reports").join("items")).map(|e| e.flatten().filter(|i| std::fs::read_to_string(i.path().join("meta.json")).is_ok_and(|m| m.contains(&format!("\"soak-op-{n}\"")))).count()).unwrap_or(0)
    };
    wait(120, || (1..=ops).all(|n| reports(n) >= 1).then_some(()));
    let applied: Vec<usize> = (1..=ops).map(reports).collect();
    let log = std::fs::read_to_string(instance.log_path()).unwrap_or_default();
    let summary = json!({
        "minutes": minutes, "cycles": cycle,
        "faults": { "none": faults[0], "freeze": faults[1], "service_restart": faults[2], "typing_into_suspended": faults[3] },
        "lines_typed": lines, "turns_in_mirror": turns,
        "commands": ops, "commands_applied_once": applied.iter().filter(|n| **n == 1).count(), "commands_lost": applied.iter().filter(|n| **n == 0).count(), "commands_doubled": applied.iter().filter(|n| **n > 1).count(),
        "links": log.matches("satellite linked").count(), "replays": log.matches("adopted again").count(), "gaps": log.matches(" gap").count(),
        "service_rss_mb_first_and_last_process": [hub_rss0, hub_rss1], "satellite_rss_mb": [sat_rss0, sat_rss1],
    });
    println!("{}", serde_json::to_string_pretty(&summary)?);
    anyhow::ensure!(turns == lines, "{lines} lines typed, {turns} turns in the mirrored history (each exactly once expected)");
    anyhow::ensure!(applied.iter().all(|n| *n == 1), "commands: {} lost, {} doubled", applied.iter().filter(|n| **n == 0).count(), applied.iter().filter(|n| **n > 1).count());
    println!("PASS: {cycle} cycles in {minutes} min; every line typed and every command exactly once");
    Ok(())
}

/// The turns (`user.message`) of session `id` in sandbox `sandbox`'s mirror.
fn mirrored_turns(home: &Path, sandbox: &str, id: &str) -> Option<u64> {
    let path: PathBuf = canvas_agents::vm::thin::mirror_dir(home, sandbox).join("copilot").join(id).join("events.jsonl");
    std::fs::read_to_string(path).ok().map(|t| t.matches("\"user.message\"").count() as u64)
}

fn wait<T>(secs: u64, mut get: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(value) = get() {
            return Some(value);
        }
        if Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}
