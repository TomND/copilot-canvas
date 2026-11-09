//! The built-in automations launcher (MAC-LINUX-PLAN D7, §5.5): what a
//! schedule runs off Windows, `atelier automations launch
//! <state>/tasks/<name>.json`. It does what the Python launcher does on
//! Windows, so the watcher, `doctor`, the health page and the canvas boxes
//! read the same things:
//!
//! - appends `Starting task.`, `Started PID N.`, `Exited with code N.` (and
//!   `Launch failed: …`) with a local timestamp to `<state>/logs/<name>.log`;
//! - gives the run `AUTOMATION_NAME`, `AUTOMATION_RUN_ID` and
//!   `AUTOMATION_STATUS_FILE`, and afterwards runs the manifest's `report`
//!   command with `AUTOMATION_EXIT_CODE` as well;
//! - enforces what a backend cannot: the time limit (the run's process group
//!   is ended), overlap (`ignore` skips a start while a run holds the task's
//!   lock, `queue` waits for it, `stop` ends the running one first,
//!   `parallel` runs side by side), restarts on failure and success codes.
//!
//! The program's own output goes to `<state>/logs/<name>.output.log`.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use canvas_sys::spawn::CommandExt;
use serde::Deserialize;

use crate::scheduler::common::{local_stamp, split_words};

/// The launcher's task file (`factory::task_json` plus the built-in
/// launcher's fields, `scheduler::common::task_json`).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Task {
    pub name: String,
    pub executable: String,
    pub arguments: String,
    pub working_directory: String,
    pub log_path: PathBuf,
    pub status_path: PathBuf,
    pub report_command: String,
    pub time_limit_ms: u64,
    pub overlap: String,
    pub restart_count: u32,
    pub restart_every_ms: u64,
    pub success_codes: Vec<i64>,
}

impl Task {
    pub fn read(path: &Path) -> Result<Task> {
        let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        let task: Task = serde_json::from_slice(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes)).with_context(|| format!("{} is not a task file", path.display()))?;
        anyhow::ensure!(!task.name.is_empty() && !task.executable.is_empty(), "{} names no task or no program", path.display());
        Ok(task)
    }

    fn locks(&self) -> PathBuf {
        self.log_path.parent().and_then(Path::parent).unwrap_or(Path::new(".")).join("locks")
    }
}

/// The runs of `name` going on now: (pid, start ms) from the pid files each
/// run keeps in `<state>/locks` (a recycled pid has another start time).
fn runs_of(locks: &Path, name: &str) -> Vec<(u32, u64)> {
    let Ok(entries) = std::fs::read_dir(locks) else { return Vec::new() };
    let prefix = format!("{name}.");
    entries
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(|f| f.starts_with(&prefix) && f.ends_with(".pid") && f[prefix.len()..f.len() - 4].parse::<u32>().is_ok()))
        .filter_map(|e| {
            let text = std::fs::read_to_string(e.path()).ok()?;
            let (pid, start) = text.trim().split_once(' ')?;
            let (pid, start) = (pid.parse().ok()?, start.parse().ok()?);
            (canvas_sys::process::process_start_ms(pid) == Some(start)).then_some((pid, start))
        })
        .collect()
}

/// Whether a run of the task (state folder `state`) is going on.
pub fn running(state: &Path, name: &str) -> bool {
    !runs_of(&state.join("locks"), name).is_empty()
}

struct Log {
    file: Option<File>,
    offset: i32,
}

impl Log {
    fn open(path: &Path) -> Log {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        Log { file: File::options().create(true).append(true).open(path).ok(), offset: canvas_sys::host::local_offset_minutes() }
    }

    fn line(&mut self, text: &str) {
        if let Some(file) = self.file.as_mut() {
            let _ = writeln!(file, "{} {text}", local_stamp(canvas_common::paths::now_ms(), self.offset));
        }
    }
}

/// Runs the task of `task_file` (in the background with `detach`: a new
/// session the scheduler does not wait for, so a timer can start the next
/// run while one goes on). Answers the exit code to leave with.
pub fn launch(task_file: &Path, detach: bool) -> Result<i32> {
    let task = Task::read(task_file)?;
    if detach {
        let exe = std::env::current_exe()?;
        Command::new(exe)
            .args(["automations", "launch"])
            .arg(task_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn_flags(canvas_sys::spawn::DETACHED)
            .spawn()
            .context("cannot start the launcher in the background")?;
        return Ok(0);
    }
    let locks = task.locks();
    std::fs::create_dir_all(&locks)?;
    let lock = File::options().create(true).truncate(false).read(true).write(true).open(locks.join(format!("{}.lock", task.name)))?;
    match task.overlap.as_str() {
        "parallel" => lock.lock_shared()?,
        "queue" => lock.lock()?,
        "stop" => {
            if lock.try_lock().is_err() {
                for (pid, _) in runs_of(&locks, &task.name) {
                    let _ = canvas_sys::process::kill_tree(pid);
                }
                lock.lock()?;
            }
        }
        // ignore: a run is going on, this start is dropped (as Task Scheduler's IgnoreNew).
        _ => {
            if lock.try_lock().is_err() {
                return Ok(0);
            }
        }
    }
    let me = std::process::id();
    let pid_file = locks.join(format!("{}.{me}.pid", task.name));
    let _ = std::fs::write(&pid_file, format!("{me} {}", canvas_sys::process::process_start_ms(me).unwrap_or(0)));
    let mut log = Log::open(&task.log_path);
    let success = if task.success_codes.is_empty() { vec![0] } else { task.success_codes.clone() };
    let mut code = 0;
    for attempt in 0..=task.restart_count {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(task.restart_every_ms.max(1000)));
        }
        code = run_once(&task, &mut log);
        if success.contains(&(code as i64)) {
            break;
        }
    }
    let _ = std::fs::remove_file(&pid_file);
    drop(lock);
    Ok(code)
}

/// The environment of a run (and of its report command).
fn environment(task: &Task, run_id: &str) -> Vec<(&'static str, String)> {
    vec![("AUTOMATION_NAME", task.name.clone()), ("AUTOMATION_RUN_ID", run_id.to_owned()), ("AUTOMATION_STATUS_FILE", task.status_path.display().to_string())]
}

fn output(task: &Task) -> (Stdio, Stdio) {
    let path = task.log_path.with_file_name(format!("{}.output.log", task.name));
    match File::options().create(true).append(true).open(path) {
        Ok(file) => match file.try_clone() {
            Ok(copy) => (Stdio::from(file), Stdio::from(copy)),
            Err(_) => (Stdio::from(file), Stdio::null()),
        },
        Err(_) => (Stdio::null(), Stdio::null()),
    }
}

/// One run: start, wait (within the time limit), log, report.
fn run_once(task: &Task, log: &mut Log) -> i32 {
    let run_id = uuid::Uuid::new_v4().to_string();
    log.line("Starting task.");
    let mut command = Command::new(&task.executable);
    command.args(split_words(&task.arguments)).stdin(Stdio::null()).spawn_flags(canvas_sys::spawn::NEW_PROCESS_GROUP);
    if !task.working_directory.is_empty() {
        command.current_dir(&task.working_directory);
    }
    for (key, value) in environment(task, &run_id) {
        command.env(key, value);
    }
    let (out, err) = output(task);
    command.stdout(out).stderr(err);
    let code = match command.spawn() {
        Err(error) => {
            log.line(&format!("Launch failed: {error}"));
            1
        }
        Ok(mut child) => {
            log.line(&format!("Started PID {}.", child.id()));
            let started = Instant::now();
            let limit = (task.time_limit_ms > 0).then(|| Duration::from_millis(task.time_limit_ms));
            let mut timed_out = false;
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break Some(status),
                    Ok(None) => {}
                    Err(_) => break None,
                }
                if !timed_out && limit.is_some_and(|l| started.elapsed() >= l) {
                    timed_out = true;
                    log.line(&format!("Stopping: the time limit of {} was reached.", crate::factory::format_ms(task.time_limit_ms)));
                    let _ = canvas_sys::process::kill_tree(child.id());
                    let _ = child.kill();
                }
                std::thread::sleep(Duration::from_millis(100));
            };
            match status {
                _ if timed_out => 124,
                Some(status) => exit_code(&status),
                None => 1,
            }
        }
    };
    log.line(&format!("Exited with code {code}."));
    if !task.report_command.trim().is_empty() {
        report(task, &run_id, code, log);
    }
    code
}

#[cfg(unix)]
fn exit_code(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

#[cfg(not(unix))]
fn exit_code(status: &std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

/// The manifest's `report` command, through the shell, after a run.
fn report(task: &Task, run_id: &str, code: i32, log: &mut Log) {
    let mut command = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/d", "/c", &task.report_command]);
        c
    } else {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", &task.report_command]);
        c
    };
    command.stdin(Stdio::null()).env("AUTOMATION_EXIT_CODE", code.to_string());
    if !task.working_directory.is_empty() {
        command.current_dir(&task.working_directory);
    }
    for (key, value) in environment(task, run_id) {
        command.env(key, value);
    }
    let (out, err) = output(task);
    command.stdout(out).stderr(err);
    match command.status() {
        Ok(status) if !status.success() => log.line(&format!("Report command exited with code {}.", exit_code(&status))),
        Ok(_) => {}
        Err(error) => log.line(&format!("Report command failed: {error}")),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn task(dir: &Path, name: &str, args: &str, extra: serde_json::Value) -> PathBuf {
        let mut json = serde_json::json!({
            "schemaVersion": 1,
            "name": name,
            "executable": "/bin/sh",
            "arguments": args,
            "workingDirectory": dir.display().to_string(),
            "logPath": dir.join("logs").join(format!("{name}.log")).display().to_string(),
            "statusPath": dir.join("status").join(format!("{name}.json")).display().to_string(),
            "reportCommand": "",
        });
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            json[k] = v;
        }
        let path = dir.join("tasks").join(format!("{name}.json"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, json.to_string()).unwrap();
        path
    }

    fn runs(dir: &Path, name: &str) -> Vec<crate::factory::LogRun> {
        crate::factory::parse_runs(&std::fs::read_to_string(dir.join("logs").join(format!("{name}.log"))).unwrap_or_default())
    }

    #[test]
    fn a_run_is_logged_with_its_environment_report_and_exit_code() {
        let dir = std::env::temp_dir().join(format!("cc-launch-{}", uuid::Uuid::new_v4().simple()));
        let file = task(
            &dir,
            "Nightly job",
            r#"-c 'echo "$AUTOMATION_NAME|$AUTOMATION_STATUS_FILE" > seen.txt; [ -n "$AUTOMATION_RUN_ID" ] && exit 3'"#,
            serde_json::json!({ "reportCommand": "echo \"code=$AUTOMATION_EXIT_CODE\" > report.txt", "successCodes": [0, 3] }),
        );
        assert_eq!(launch(&file, false).unwrap(), 3);
        let seen = std::fs::read_to_string(dir.join("seen.txt")).unwrap();
        assert_eq!(seen.trim(), format!("Nightly job|{}", dir.join("status").join("Nightly job.json").display()));
        assert_eq!(std::fs::read_to_string(dir.join("report.txt")).unwrap().trim(), "code=3");
        let log = std::fs::read_to_string(dir.join("logs").join("Nightly job.log")).unwrap();
        assert!(log.lines().next().unwrap().ends_with(" Starting task.") && log.contains(" Started PID ") && log.contains(" Exited with code 3."), "{log}");
        assert_eq!(runs(&dir, "Nightly job")[0].exit, Some(3));
        assert!(!running(&dir, "Nightly job"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn failures_restart_time_limits_stop_and_launch_failures_are_logged() {
        let dir = std::env::temp_dir().join(format!("cc-launch-{}", uuid::Uuid::new_v4().simple()));
        let file = task(&dir, "flaky", "-c 'exit 2'", serde_json::json!({ "restartCount": 2, "restartEveryMs": 1000 }));
        assert_eq!(launch(&file, false).unwrap(), 2);
        assert_eq!(runs(&dir, "flaky").len(), 3, "the first run and two restarts");
        let file = task(&dir, "slow", "-c 'sleep 30'", serde_json::json!({ "timeLimitMs": 500 }));
        let started = Instant::now();
        assert_eq!(launch(&file, false).unwrap(), 124);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(std::fs::read_to_string(dir.join("logs").join("slow.log")).unwrap().contains("Stopping: the time limit"));
        let file = task(&dir, "missing", "", serde_json::json!({ "executable": dir.join("nope").display().to_string() }));
        assert_eq!(launch(&file, false).unwrap(), 1);
        assert!(!runs(&dir, "missing")[0].error.is_empty(), "the launch failure is the run's error");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn overlapping_starts_follow_the_overlap_setting() {
        let dir = std::env::temp_dir().join(format!("cc-launch-{}", uuid::Uuid::new_v4().simple()));
        let file = task(&dir, "long", "-c 'sleep 2'", serde_json::json!({}));
        let first = {
            let file = file.clone();
            std::thread::spawn(move || launch(&file, false).unwrap())
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !running(&dir, "long") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(running(&dir, "long"), "the first run holds the lock");
        assert_eq!(launch(&file, false).unwrap(), 0, "ignore: the second start is dropped");
        assert_eq!(first.join().unwrap(), 0);
        assert_eq!(runs(&dir, "long").len(), 1, "one run only");
        let _ = std::fs::remove_dir_all(dir);
    }
}
