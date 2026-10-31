//! `copilot-canvas selftest automations`: the automations commands end to end
//! on this machine's scheduler (Task Scheduler, systemd, cron, launchd), in a
//! throw-away folder: apply (preview, then for real), list, run now, a
//! scheduled run (every minute), the launcher's log and status report,
//! disable and enable, drift after a manifest edit, import back to a manifest
//! and removal. The task it installs is removed at the end, also on failure;
//! nothing else of the user's schedule is touched. Meant for CI runners (and
//! a developer who wants one probe task registered for about three minutes).

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use canvas_agents::{factory, scheduler};

struct Probe {
    exe: PathBuf,
    root: PathBuf,
    home: PathBuf,
    dir: PathBuf,
    state: PathBuf,
    work: PathBuf,
    name: String,
}

impl Probe {
    /// `copilot-canvas --home <probe home> ARGS…`: its output, or an error with it.
    fn cli(&self, args: &[&str]) -> Result<String> {
        let out = Command::new(&self.exe).arg("--home").arg(&self.home).args(args).output()?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        anyhow::ensure!(out.status.success(), "copilot-canvas {} failed:\n{text}", args.join(" "));
        Ok(text)
    }

    fn dir_arg(&self) -> String {
        self.dir.display().to_string()
    }

    fn manifest_path(&self) -> PathBuf {
        self.dir.join("probe").join(format!("{}.yaml", self.name))
    }

    fn write_manifest(&self, schedule: &[&str]) -> Result<()> {
        let q = |s: &str| format!("'{}'", s.replace('\'', "''"));
        let ran = self.work.join("ran.txt");
        let (command, args) = ("/bin/sh".to_owned(), format!("-c 'echo \"ran $AUTOMATION_NAME\" >> \"{}\"; exit 3'", ran.display()));
        let report = format!("'{}' --home '{}' automations report alert \"exit code $AUTOMATION_EXIT_CODE\"", self.exe.display(), self.home.display());
        let schedule: String = schedule.iter().map(|s| format!("  - {}\n", q(s))).collect();
        std::fs::create_dir_all(self.manifest_path().parent().context("family folder")?)?;
        std::fs::write(
            self.manifest_path(),
            format!(
                "name: {}\nfamily: 'Probe'\nenabled: true\ncommand: {}\nargs: {}\ncwd: {}\nschedule:\n{schedule}time_limit: '5m'\nreport: {}\n",
                q(&self.name),
                q(&command),
                q(&args),
                q(&self.work.display().to_string()),
                q(&report)
            ),
        )?;
        Ok(())
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.state.join("logs").join(format!("{}.log", self.name))).unwrap_or_default()
    }

    fn task(&self) -> Result<Option<scheduler::TaskInfo>> {
        Ok(scheduler::get().list()?.remove(&self.name.to_lowercase()))
    }

    fn wait(&self, what: &str, timeout: Duration, ok: impl Fn(&Probe) -> bool) -> Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if ok(self) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        bail!("timed out waiting for {what}; launcher log:\n{}", self.log())
    }
}

fn step(text: &str) {
    println!("  · {text}");
}

pub fn run() -> Result<()> {
    anyhow::ensure!(canvas_sys::host::platform() != canvas_sys::host::Platform::Windows, "on Windows the Task Scheduler probe is `cargo test factory_probe -- --ignored`");
    let root = std::env::temp_dir().join(format!("copilot-canvas-automations-{}", uuid::Uuid::new_v4().simple()));
    let probe = Probe {
        exe: std::env::current_exe()?,
        home: root.join("home"),
        dir: root.join("automations"),
        state: root.join("state"),
        work: root.join("work"),
        root,
        name: format!("CanvasProbe-{}", std::process::id()),
    };
    let sched = scheduler::get();
    println!("Copilot Canvas automations self-test on {} (task {}; data in {})", sched.name(), probe.name, probe.root.display());
    for dir in [&probe.home, &probe.dir, &probe.work] {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(probe.dir.join(factory::DEFAULTS_FILE), format!("state: '{}'\n", probe.state.display().to_string().replace('\'', "''")))?;
    let result = scenario(&probe);
    // Always take the probe task out of the scheduler.
    if probe.task().ok().flatten().is_some() {
        let _ = sched.delete(&probe.name);
    }
    match &result {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&probe.root);
        }
        Err(_) => eprintln!("artifacts kept in {}", probe.root.display()),
    }
    result
}

fn scenario(p: &Probe) -> Result<()> {
    let sched = scheduler::get();
    let started = Instant::now();
    p.write_manifest(&["every 1m", "daily 03:17"])?;

    step("apply previews a new task, apply --yes installs it without drift");
    let preview = p.cli(&["automations", "apply", "--dir", &p.dir_arg()])?;
    anyhow::ensure!(preview.contains(&format!("~ {}", p.name)) && preview.contains("new task"), "preview:\n{preview}");
    anyhow::ensure!(p.task()?.is_none(), "a preview installs nothing");
    let applied = p.cli(&["automations", "apply", "--dir", &p.dir_arg(), "--yes"])?;
    anyhow::ensure!(applied.contains("    applied") && !applied.contains("still differs"), "apply:\n{applied}");
    let again = p.cli(&["automations", "apply", "--dir", &p.dir_arg()])?;
    anyhow::ensure!(again.contains(&format!("= {}", p.name)), "a second apply finds nothing to do:\n{again}");
    let task = p.task()?.context("the task is installed")?;
    anyhow::ensure!(task.enabled && task.state != "disabled", "installed enabled: {task:?}");
    anyhow::ensure!(task.next_run_ms > canvas_common::paths::now_ms(), "a next run is known: {task:?}");
    let listed = p.cli(&["automations", "list", "--dir", &p.dir_arg()])?;
    anyhow::ensure!(listed.contains(&p.name) && !listed.contains("drift:"), "list:\n{listed}");

    step("run now goes through the launcher: log lines, environment, exit code, report");
    p.cli(&["automations", "run", &p.name, "--dir", &p.dir_arg()])?;
    p.wait("the run's exit line", Duration::from_secs(60), |p| p.log().contains("Exited with code 3."))?;
    let runs = factory::parse_runs(&p.log());
    anyhow::ensure!(runs.first().is_some_and(|r| r.exit == Some(3)), "the newest run exited with 3: {runs:?}");
    let ran = std::fs::read_to_string(p.work.join("ran.txt")).unwrap_or_default();
    anyhow::ensure!(ran.contains(&format!("ran {}", p.name)), "AUTOMATION_NAME reached the run: {ran:?}");
    p.wait("the status report", Duration::from_secs(20), |p| {
        factory::read_report(&p.state.join("status").join(format!("{}.json", p.name))).is_some_and(|r| r.state == "alert" && r.headline == "exit code 3")
    })?;
    let shown = p.cli(&["automations", "show", &p.name, "--dir", &p.dir_arg()])?;
    anyhow::ensure!(shown.contains("REPORT alert · exit code 3") && shown.contains("exit 3"), "show:\n{shown}");

    step("the schedule starts a run by itself (every minute)");
    let before = factory::parse_runs(&p.log()).len();
    p.wait("a scheduled run", Duration::from_secs(150), |p| factory::parse_runs(&p.log()).len() > before && p.log().matches("Exited with code").count() > before)?;

    step("disable and enable: the manifest and the scheduler follow");
    p.cli(&["automations", "disable", &p.name, "--dir", &p.dir_arg()])?;
    anyhow::ensure!(std::fs::read_to_string(p.manifest_path())?.contains("enabled: false"), "the manifest says disabled");
    let task = p.task()?.context("still installed")?;
    anyhow::ensure!(!task.enabled && task.state == "disabled", "disabled: {task:?}");
    let listed = p.cli(&["automations", "list", "--dir", &p.dir_arg()])?;
    anyhow::ensure!(!listed.contains("drift:"), "no drift once disabled:\n{listed}");
    p.cli(&["automations", "enable", &p.name, "--dir", &p.dir_arg()])?;
    anyhow::ensure!(p.task()?.is_some_and(|t| t.enabled), "enabled again");

    step("a manifest edit shows as drift until applied");
    p.write_manifest(&["daily 05:40", "weekly mon,fri 18:05"])?;
    let preview = p.cli(&["automations", "apply", "--dir", &p.dir_arg()])?;
    anyhow::ensure!(preview.contains("schedule:") && preview.contains("daily 05:40"), "drift:\n{preview}");
    let applied = p.cli(&["automations", "apply", "--dir", &p.dir_arg(), "--yes"])?;
    anyhow::ensure!(applied.contains("    applied") && !applied.contains("still differs"), "apply:\n{applied}");

    step("import writes the installed task back as a manifest");
    let imported_dir = p.root.join("imported");
    p.cli(&["automations", "import", &p.name, "--dir", &imported_dir.display().to_string(), "--family", "Probe"])?;
    let repo = factory::load_repo(&imported_dir);
    let original = factory::load_repo(&p.dir);
    let (a, b) = (repo.manifests.first().context("an imported manifest")?, original.manifests.first().context("the manifest")?);
    anyhow::ensure!((a.command.as_str(), a.args.as_str(), &a.schedule) == (b.command.as_str(), b.args.as_str(), &b.schedule), "imported {a:?}\n  from {b:?}");
    // Written elsewhere, it is the same task (only its file's path differs).
    let mut same = a.clone();
    same.path = b.path.clone();
    let drift = sched.drift(&same, &original.defaults, &p.task()?.context("installed")?);
    anyhow::ensure!(drift.is_empty(), "the imported manifest matches the task: {drift:?}");

    step("remove takes it out of the scheduler (its manifest stays)");
    p.cli(&["automations", "remove", &p.name, "--dir", &p.dir_arg()])?;
    anyhow::ensure!(p.task()?.is_none() && p.manifest_path().is_file(), "removed");
    println!("PASS in {:.1}s on {}", started.elapsed().as_secs_f64(), sched.name());
    Ok(())
}

