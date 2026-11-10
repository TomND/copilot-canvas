//! `vm::local`: sandboxes as folders and local processes, for tests and CI
//! (never a real VM). Each sandbox is `<root>/sandboxes/<id>/` with its own
//! `home` (`HOME`, `ATELIER_HOME`, `COPILOT_HOME`, … all point inside it, so
//! nothing reaches the real user's folders), the guest's whole disk, with
//! the workspace `home/work/<repo>` where the guest clones (`guest::workspace`;
//! the stand-in's claim clones there when the repository's URL is a local
//! repository, else makes an empty folder).
//!
//! [`Local`] is a complete [`Provider`] on Windows, macOS and Linux:
//! - `state.json` holds the record: labels, `Running` or `Suspended`;
//! - `processes.json` holds the pid and start time of every process started
//!   through [`Local::exec`], [`Local::spawn`] or [`Local::track`], and of
//!   their descendants, recorded again at each call (so a process that leaves
//!   its parent stays known). Suspend freezes them all with
//!   `canvas_sys::process::suspend_tree` (as a memory suspend stops a VM),
//!   resume thaws them, delete ends them and then removes the folder; a pid
//!   is checked against its start time before every signal;
//! - the guest's disk for `put_file`/`get_file` is the sandbox's `home/`
//!   (relative paths are from it, as from a guest's home; absolute paths
//!   must be inside it; anything else and links leading out are refused);
//! - images are folders `<root>/images/<id>/` (with `<id>.json` beside) of
//!   a workspace, copied into `home/work/` when a sandbox is made from one;
//! - the egress policy is `egress.json` and the egress stand-in's decisions
//!   `egress-decisions.jsonl`, in the sandbox folder but outside its disk;
//! - secrets are `<root>/secrets/<name>.json` (owner-only), never under a
//!   sandbox folder and never in a process's environment.
//!
//! Test knobs, read only here: `ATELIER_VM_TEST_FAIL=create:1,resume:2`
//! fails the Nth call of that kind (counted per root, across processes, so
//! the stand-in's hook processes count together) and
//! `ATELIER_VM_TEST_LATENCY_MS` sleeps before every provider call. A sandbox
//! lost behind the hub's back is [`Local::delete`] (no knob, no count).
//!
//! In Phase M it backs `atelier vm-hook-standin`, which answers the pack's
//! hooks the way real ones would, so the whole thin lane runs on any machine.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::provider::{self, EgressDecision, EgressPolicy, ExecResult, ImageRef, Labels, Provider, SandboxSpec, Secret};
use crate::paths::now_ms;

/// A sandbox's record (`state.json`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Sandbox {
    pub id: String,
    pub r#box: String,
    pub repo: String,
    pub branch: String,
    pub cwd: String,
    pub created_ms: u64,
    pub suspended: bool,
    /// `Running`, `Suspended` or `Deleting` (empty in older records: `suspended` tells).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub state: String,
    #[serde(skip_serializing_if = "Labels::is_empty")]
    pub labels: Labels,
    /// The image it was made from.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub image: String,
    /// Plain environment of every process in it (`SandboxSpec::env`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<(String, String)>,
}

impl Sandbox {
    /// `Running`, `Suspended` or `Deleting`.
    pub fn state(&self) -> &str {
        match self.state.as_str() {
            "" if self.suspended => "Suspended",
            "" => "Running",
            state => state,
        }
    }

    fn set_state(&mut self, state: &str) {
        self.state = state.to_owned();
        self.suspended = state == "Suspended";
    }

    /// The sandbox as the provider reports it.
    pub fn summary(&self) -> provider::Sandbox {
        provider::Sandbox { id: self.id.clone(), state: self.state().to_owned(), labels: self.labels.clone() }
    }
}

/// A process of a sandbox (`processes.json`): its pid and start time, which
/// must still match before it is signalled.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    pub start_ms: u64,
}

impl Proc {
    fn alive(&self) -> bool {
        canvas_sys::process::process_start_ms(self.pid) == Some(self.start_ms)
    }
}

/// The provider calls the failure knob counts.
const CALLS: [&str; 17] = [
    "create", "get", "list", "exec", "put_file", "get_file", "suspend", "resume", "delete", "set_egress", "egress_decisions", "image_build", "image_commit", "image_delete", "images", "secret_set", "start_satellite",
];

/// Failure injection for tests (`ATELIER_VM_TEST_FAIL`, `ATELIER_VM_TEST_LATENCY_MS`).
#[derive(Clone, Debug, Default, PartialEq)]
struct Faults {
    /// (call, the Nth call of it that fails).
    fail: Vec<(String, u64)>,
    latency_ms: u64,
    /// A knob that does not parse: every call fails with this, so a test notices.
    malformed: Option<String>,
}

impl Faults {
    fn parse(fail: &str, latency_ms: &str) -> Result<Faults> {
        let mut faults = Faults::default();
        for entry in fail.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let (call, nth) = entry.split_once(':').with_context(|| format!("{entry:?} is not CALL:N"))?;
            anyhow::ensure!(CALLS.contains(&call), "{call:?} is not a provider call ({})", CALLS.join(", "));
            let nth = nth.parse::<u64>().ok().filter(|n| *n > 0).with_context(|| format!("{entry:?}: N counts calls from 1"))?;
            faults.fail.push((call.to_owned(), nth));
        }
        if !latency_ms.trim().is_empty() {
            faults.latency_ms = latency_ms.trim().parse().context("ATELIER_VM_TEST_LATENCY_MS is a number of milliseconds")?;
        }
        Ok(faults)
    }
}

pub struct Local {
    pub root: PathBuf,
    faults: Faults,
}

/// A sandbox id: `sbx-` and 12 hex digits.
pub fn valid_id(id: &str) -> bool {
    id.len() == 16 && id.starts_with("sbx-") && id[4..].chars().all(|c| c.is_ascii_hexdigit())
}

/// A name that is one plain file or folder name (an image id, a secret's name).
fn plain_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 128 && !name.starts_with('.') && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", &uuid::Uuid::new_v4().simple().to_string()[..12])
}

/// An exclusive lock on `path` (created when missing), held until the file is dropped.
fn lock(path: &Path) -> Result<std::fs::File> {
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path).with_context(|| format!("cannot open {}", path.display()))?;
    file.lock().with_context(|| format!("cannot lock {}", path.display()))?;
    Ok(file)
}

/// Copies the folder `from` into `to` (links are copied as links on Unix and
/// skipped on Windows).
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from).with_context(|| format!("cannot read {}", from.display()))? {
        let entry = entry?;
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&source, &target)?;
        } else if kind.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(&source)?, &target)?;
        } else {
            std::fs::copy(&source, &target).with_context(|| format!("cannot copy {}", source.display()))?;
        }
    }
    Ok(())
}

/// The workspace of the sandbox in `dir`: `~/work` of its guest.
fn workspace(dir: &Path) -> PathBuf {
    dir.join("home").join("work")
}

fn read_processes(dir: &Path) -> Vec<Proc> {
    std::fs::read(dir.join("processes.json")).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

/// Every process's children (by parent pid).
fn children() -> HashMap<u32, Vec<u32>> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, parent, _) in canvas_sys::process::process_entries() {
        children.entry(parent).or_default().push(pid);
    }
    children
}

/// The sandbox's live processes (the caller holds its lock): the recorded
/// ones that still run with their start time, then their descendants now,
/// which are recorded too.
fn refresh(dir: &Path) -> Result<Vec<Proc>> {
    let mut procs = read_processes(dir);
    procs.retain(Proc::alive);
    let children = children();
    let roots: Vec<u32> = procs.iter().map(|p| p.pid).collect();
    for root in roots {
        for pid in canvas_sys::process::process_tree(root, &children) {
            if !procs.iter().any(|p| p.pid == pid)
                && let Some(start_ms) = canvas_sys::process::process_start_ms(pid)
            {
                procs.push(Proc { pid, start_ms });
            }
        }
    }
    crate::paths::write_atomic(&dir.join("processes.json"), &serde_json::to_vec_pretty(&procs)?)?;
    Ok(procs)
}

/// The processes no other one of `procs` has in its tree now: each tree is
/// frozen and thawed once from its top (Windows counts suspensions).
fn tops(procs: &[Proc]) -> Vec<Proc> {
    let children = children();
    let below: HashSet<u32> = procs.iter().flat_map(|p| canvas_sys::process::process_tree(p.pid, &children).into_iter().skip(1)).collect();
    procs.iter().filter(|p| !below.contains(&p.pid)).copied().collect()
}

/// Records `pid` as a process of the sandbox in `dir` (the caller holds its
/// lock); a process that has already ended needs no record.
fn record(dir: &Path, pid: u32) -> Result<()> {
    let Some(start_ms) = canvas_sys::process::process_start_ms(pid) else { return Ok(()) };
    let mut procs = read_processes(dir);
    procs.retain(Proc::alive);
    if !procs.iter().any(|p| p.pid == pid) {
        procs.push(Proc { pid, start_ms });
    }
    crate::paths::write_atomic(&dir.join("processes.json"), &serde_json::to_vec_pretty(&procs)?)?;
    Ok(())
}

impl Local {
    /// A provider on `root`, with the test knobs of the environment.
    pub fn new(root: PathBuf) -> Self {
        let knob = |name: &str| canvas_common::env::var(name).unwrap_or_default();
        let faults = Faults::parse(&knob("ATELIER_VM_TEST_FAIL"), &knob("ATELIER_VM_TEST_LATENCY_MS")).unwrap_or_else(|error| Faults { malformed: Some(format!("{error:#}")), ..Default::default() });
        Local { root, faults }
    }

    /// A provider whose test knobs are given here instead of by the
    /// environment: `fail` as `ATELIER_VM_TEST_FAIL`, `latency_ms` as
    /// `ATELIER_VM_TEST_LATENCY_MS`.
    pub fn with_faults(root: PathBuf, fail: &str, latency_ms: u64) -> Result<Self> {
        let faults = Faults { latency_ms, ..Faults::parse(fail, "")? };
        Ok(Local { root, faults })
    }

    /// Before each provider call: the latency knob, then the failure knob.
    fn before(&self, call: &str) -> Result<()> {
        if self.faults.latency_ms > 0 {
            std::thread::sleep(Duration::from_millis(self.faults.latency_ms));
        }
        if let Some(error) = &self.faults.malformed {
            bail!("ATELIER_VM_TEST_FAIL or ATELIER_VM_TEST_LATENCY_MS is malformed: {error}");
        }
        if self.faults.fail.is_empty() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.root)?;
        let _lock = lock(&self.root.join("test-calls.lock"))?;
        let path = self.root.join("test-calls.json");
        let mut counts: HashMap<String, u64> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let nth = counts.entry(call.to_owned()).or_default();
        *nth += 1;
        let nth = *nth;
        crate::paths::write_atomic(&path, &serde_json::to_vec(&counts)?)?;
        if self.faults.fail.iter().any(|(c, n)| c == call && *n == nth) {
            bail!("injected failure (ATELIER_VM_TEST_FAIL): {call} call {nth}");
        }
        Ok(())
    }

    fn dir(&self, id: &str) -> Result<PathBuf> {
        anyhow::ensure!(valid_id(id), "not a sandbox id: {id:?}");
        let dir = self.root.join("sandboxes").join(id);
        anyhow::ensure!(dir.join("state.json").is_file(), "no sandbox {id}");
        Ok(dir)
    }

    pub fn get(&self, id: &str) -> Result<Sandbox> {
        let dir = self.dir(id)?;
        Ok(serde_json::from_slice(&std::fs::read(dir.join("state.json"))?)?)
    }

    fn save(&self, sandbox: &Sandbox) -> Result<()> {
        let path = self.root.join("sandboxes").join(&sandbox.id).join("state.json");
        crate::paths::write_atomic(&path, &serde_json::to_vec_pretty(sandbox)?)?;
        Ok(())
    }

    pub fn list(&self) -> Vec<Sandbox> {
        let Ok(entries) = std::fs::read_dir(self.root.join("sandboxes")) else { return Vec::new() };
        let mut out: Vec<Sandbox> = entries.flatten().filter_map(|e| std::fs::read(e.path().join("state.json")).ok()).filter_map(|b| serde_json::from_slice(&b).ok()).collect();
        out.sort_by(|a: &Sandbox, b: &Sandbox| a.id.cmp(&b.id));
        out
    }

    /// A new sandbox for `box` with `repo` cloned from `url` (when it is a
    /// local repository) on `branch` from `base` (tracking the remote branch
    /// when it exists).
    pub fn claim(&self, r#box: &str, repo: &str, url: &str, branch: &str, base: &str) -> Result<Sandbox> {
        anyhow::ensure!(canvas_model::vm::sync_name_ok(repo) && !repo.contains('/'), "not a repository key: {repo:?}");
        let id = new_id("sbx");
        let dir = self.root.join("sandboxes").join(&id);
        for sub in ["home/.atelier", "home/.copilot", "home/work"] {
            std::fs::create_dir_all(dir.join(sub))?;
        }
        let work = workspace(&dir).join(repo);
        let local_repo = !url.contains("://") && Path::new(url).exists();
        if local_repo {
            let git = |args: &[&str], cwd: &Path| -> Result<bool> {
                let mut cmd = Command::new("git");
                cmd.args(args).current_dir(cwd).env("GIT_TERMINAL_PROMPT", "0");
                canvas_sys::spawn::CommandExt::no_window(&mut cmd);
                Ok(cmd.output().context("git is not installed")?.status.success())
            };
            if !git(&["clone", "--quiet", url, &work.display().to_string()], &dir)? {
                bail!("cannot clone {repo} into the sandbox");
            }
            let tracked = git(&["checkout", "--quiet", "-b", branch, "--track", &format!("origin/{branch}")], &work)?;
            if !tracked && !git(&["checkout", "--quiet", "-b", branch, &format!("origin/{base}")], &work)? && !git(&["checkout", "--quiet", "-b", branch], &work)? {
                bail!("cannot make the branch {branch}");
            }
        } else {
            std::fs::create_dir_all(&work)?;
        }
        let labels = Labels::from([("atelier.box".to_owned(), r#box.to_owned()), ("atelier.repo".to_owned(), repo.to_owned())]);
        let mut sandbox = Sandbox { id, r#box: r#box.to_owned(), repo: repo.to_owned(), branch: branch.to_owned(), cwd: work.display().to_string(), created_ms: now_ms(), labels, ..Default::default() };
        sandbox.set_state("Running");
        self.save(&sandbox)?;
        Ok(sandbox)
    }

    /// Ends the sandbox's processes (frozen first, so none can start another
    /// meanwhile), then removes its folder. Tests also use it to lose a
    /// sandbox behind the hub's back.
    pub fn delete(&self, id: &str) -> Result<()> {
        let dir = self.dir(id)?;
        {
            let _lock = lock(&dir.join("lock"))?;
            let mut sandbox = self.get(id)?;
            sandbox.set_state("Deleting");
            self.save(&sandbox)?;
            let procs = refresh(&dir)?;
            for top in tops(&procs).into_iter().rev() {
                let _ = canvas_sys::process::suspend_tree(top.pid, top.start_ms);
            }
            for proc in procs.iter().rev() {
                if proc.alive() {
                    let _ = canvas_sys::process::kill_tree(proc.pid);
                }
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            while procs.iter().any(Proc::alive) {
                anyhow::ensure!(Instant::now() < deadline, "the processes of sandbox {id} did not end");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        // Windows: a process that just ended may hold its folder a moment longer.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) if Instant::now() > deadline => return Err(error).with_context(|| format!("cannot remove sandbox {id}")),
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }

    /// Sets the record's state only (no process is frozen or thawed).
    pub fn set_suspended(&self, id: &str, suspended: bool) -> Result<()> {
        let _lock = lock(&self.dir(id)?.join("lock"))?;
        let mut sandbox = self.get(id)?;
        sandbox.set_state(if suspended { "Suspended" } else { "Running" });
        self.save(&sandbox)
    }

    /// The environment a process in the sandbox gets: its own home for
    /// everything (also on Windows, whose user folders do not follow `HOME`).
    pub fn env(&self, id: &str) -> Result<Vec<(String, String)>> {
        let home = self.dir(id)?.join("home");
        let h = |sub: &str| home.join(sub).display().to_string();
        let exe_dir = std::env::current_exe()?.parent().map(Path::to_path_buf).unwrap_or_default();
        let path = std::env::join_paths(std::iter::once(exe_dir).chain(std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default()))?;
        Ok(vec![
            ("HOME".into(), home.display().to_string()),
            ("USERPROFILE".into(), home.display().to_string()),
            ("ATELIER_HOME".into(), h(".atelier")),
            ("COPILOT_HOME".into(), h(".copilot")),
            ("XDG_DATA_HOME".into(), h(".local/share")),
            ("XDG_CONFIG_HOME".into(), h(".config")),
            ("CLAUDE_CONFIG_DIR".into(), h(".claude")),
            ("PATH".into(), path.to_string_lossy().into_owned()),
        ])
    }

    /// A command in the sandbox: its program (`atelier` is this program; an
    /// agent's name follows `ATELIER_<AGENT>` when it is set, as the
    /// self-tests' stand-in agents do), with the sandbox's environment.
    /// Start it with [`Local::spawn`] (or record it with [`Local::track`]) so
    /// that suspend and delete reach it.
    pub fn command(&self, id: &str, argv: &[String], cwd: Option<&str>, env: &[(String, String)]) -> Result<Command> {
        let program = argv.first().context("nothing to run")?;
        let program = match program.as_str() {
            "atelier" => std::env::current_exe()?.display().to_string(),
            name @ ("copilot" | "opencode" | "claude") => canvas_common::env::var(&format!("ATELIER_{}", name.to_ascii_uppercase())).unwrap_or_else(|_| name.to_owned()),
            other => other.to_owned(),
        };
        let mut cmd = Command::new(program);
        cmd.args(&argv[1..]);
        let sandbox = self.get(id)?;
        cmd.current_dir(cwd.filter(|c| Path::new(c).is_dir()).unwrap_or(&sandbox.cwd));
        // Nothing of the hub's own service or pack reaches the guest.
        for key in ["ATELIER_VM_PACK", "ATELIER_LISTEN", "ATELIER_INSTANCE", "ATELIER_SESSION", "ATELIER_PASSIVE", "ATELIER_REMOTE"] {
            cmd.env_remove(key);
            if let Some(old) = canvas_common::env::legacy_name(key) {
                cmd.env_remove(old);
            }
        }
        // The sandbox's plain environment, then its homes (which it cannot move), then the caller's.
        for (k, v) in sandbox.env.iter().cloned().chain(self.env(id)?).chain(env.iter().cloned()) {
            cmd.env(k, v);
        }
        Ok(cmd)
    }

    /// Starts `cmd` (from [`Local::command`]) as a process of the sandbox,
    /// recorded so that suspend freezes it and delete ends it. Refused unless
    /// the sandbox is running.
    pub fn spawn(&self, id: &str, cmd: &mut Command) -> Result<Child> {
        let dir = self.dir(id)?;
        let _lock = lock(&dir.join("lock"))?;
        let sandbox = self.get(id)?;
        anyhow::ensure!(sandbox.state() == "Running", "sandbox {id} is {}", sandbox.state().to_ascii_lowercase());
        let mut child = cmd.spawn()?;
        if let Err(error) = record(&dir, child.id()) {
            // Never a process of the sandbox that suspend and delete cannot reach.
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        Ok(child)
    }

    /// Records `pid`, a process started in the sandbox some other way (a
    /// terminal's), as one of its processes.
    pub fn track(&self, id: &str, pid: u32) -> Result<()> {
        let dir = self.dir(id)?;
        let _lock = lock(&dir.join("lock"))?;
        record(&dir, pid)
    }

    /// The sandbox's live processes, with their descendants now (recorded too).
    pub fn processes(&self, id: &str) -> Result<Vec<Proc>> {
        let dir = self.dir(id)?;
        let _lock = lock(&dir.join("lock"))?;
        refresh(&dir)
    }

    /// Runs a command in the sandbox to its end: (exit code, stdout, stderr).
    pub fn exec(&self, id: &str, argv: &[String], stdin: &[u8]) -> Result<(i64, Vec<u8>, String)> {
        use std::io::Write;
        let mut cmd = self.command(id, argv, None, &[])?;
        cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
        canvas_sys::spawn::CommandExt::no_window(&mut cmd);
        let mut child = self.spawn(id, &mut cmd).with_context(|| format!("cannot run {} in the sandbox", argv[0]))?;
        if let Some(mut input) = child.stdin.take() {
            let bytes = stdin.to_vec();
            std::thread::spawn(move || {
                let _ = input.write_all(&bytes);
            });
        }
        let out = child.wait_with_output()?;
        Ok((out.status.code().unwrap_or(-1) as i64, out.stdout, String::from_utf8_lossy(&out.stderr).into_owned()))
    }

    /// The file `path` names on the sandbox's disk, its `home/` (the
    /// guest's home): relative to it (`.atelier/x`) or absolute inside it.
    /// `..`, other absolute paths and links leading out of `home/` are
    /// refused, so the sandbox's own files are out of reach. `create` makes
    /// the missing folders on the way, never through a link.
    fn guest_file(&self, id: &str, path: &str, create: bool) -> Result<PathBuf> {
        let home = self.dir(id)?.join("home");
        let refused = || anyhow!("{path:?} is outside the sandbox's disk (its home)");
        let base = home.canonicalize()?;
        let given = Path::new(path);
        let relative = if given.has_root() || given.is_absolute() {
            given.strip_prefix(&home).or_else(|_| given.strip_prefix(&base)).map_err(|_| refused())?
        } else {
            given
        };
        let mut parts = Vec::new();
        for component in relative.components() {
            match component {
                Component::Normal(part) => parts.push(part),
                Component::CurDir => {}
                _ => return Err(refused()),
            }
        }
        let (name, folders) = parts.split_last().ok_or_else(refused)?;
        let mut at = base.clone();
        for part in folders {
            let next = at.join(part);
            match std::fs::symlink_metadata(&next) {
                Ok(meta) if meta.is_symlink() => {
                    let real = next.canonicalize().map_err(|_| refused())?;
                    if !real.starts_with(&base) || !real.is_dir() {
                        return Err(refused());
                    }
                    at = real;
                }
                Ok(meta) if meta.is_dir() => at = next,
                Ok(_) => bail!("{path:?}: {} is not a folder", part.to_string_lossy()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
                    std::fs::create_dir(&next)?;
                    at = next;
                }
                Err(error) => return Err(anyhow!("{path:?}: {error}")),
            }
        }
        let file = at.join(name);
        if std::fs::symlink_metadata(&file).is_ok_and(|meta| meta.is_symlink()) {
            let real = file.canonicalize().map_err(|_| refused())?;
            if !real.starts_with(&base) {
                return Err(refused());
            }
            return Ok(real);
        }
        Ok(file)
    }

    /// The sandbox's egress policy (what the egress stand-in enforces).
    pub fn egress(&self, id: &str) -> Result<Option<EgressPolicy>> {
        match std::fs::read(self.dir(id)?.join("egress.json")) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Appends one of the egress stand-in's decisions to `egress-decisions.jsonl`.
    pub fn log_egress(&self, id: &str, decision: &EgressDecision) -> Result<()> {
        use std::io::Write;
        let dir = self.dir(id)?;
        let _lock = lock(&dir.join("lock"))?;
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("egress-decisions.jsonl"))?;
        writeln!(file, "{}", serde_json::to_string(decision)?)?;
        Ok(())
    }

    /// A secret's value (for the egress stand-in, which injects it; nothing
    /// in a sandbox can read it).
    pub fn secret(&self, name: &str, key: &str) -> Result<Option<Secret>> {
        anyhow::ensure!(plain_name(name), "not a secret name: {name:?}");
        let bytes = match std::fs::read(self.root.join("secrets").join(format!("{name}.json"))) {
            Ok(bytes) => Secret::new(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let values: serde_json::Map<String, Value> = serde_json::from_slice(bytes.expose()).context("a secrets file that does not parse")?;
        Ok(values.get(key).and_then(Value::as_str).map(|v| Secret::new(v.as_bytes().to_vec())))
    }

    /// A new image folder (a copy of `from` when given) and its `<id>.json`.
    fn new_image(&self, name: &str, from: Option<&Path>) -> Result<ImageRef> {
        let images = self.root.join("images");
        let image = ImageRef { id: new_id("img"), name: name.to_owned(), created_ms: now_ms() };
        let dir = images.join(&image.id);
        std::fs::create_dir_all(&dir)?;
        if let Some(from) = from
            && let Err(error) = copy_tree(from, &dir)
        {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(error);
        }
        crate::paths::write_atomic(&images.join(format!("{}.json", image.id)), &serde_json::to_vec_pretty(&image)?)?;
        Ok(image)
    }
}

impl Provider for Local {
    fn name(&self) -> &'static str {
        "local"
    }

    fn create(&self, spec: &SandboxSpec) -> Result<provider::Sandbox> {
        self.before("create")?;
        let image = match spec.image.as_deref() {
            Some(image) => {
                anyhow::ensure!(plain_name(image), "not an image id: {image:?}");
                let folder = self.root.join("images").join(image);
                anyhow::ensure!(folder.is_dir(), "no image {image}");
                Some(folder)
            }
            None => None,
        };
        let id = new_id("sbx");
        let dir = self.root.join("sandboxes").join(&id);
        let made = (|| -> Result<provider::Sandbox> {
            for sub in ["home/.atelier", "home/.copilot", "home/work"] {
                std::fs::create_dir_all(dir.join(sub))?;
            }
            let work = workspace(&dir);
            if let Some(image) = &image {
                copy_tree(image, &work)?;
            }
            if let Some(policy) = &spec.egress {
                crate::paths::write_atomic(&dir.join("egress.json"), &serde_json::to_vec_pretty(policy)?)?;
            }
            let label = |key: &str| spec.labels.get(key).cloned().unwrap_or_default();
            let mut sandbox = Sandbox {
                id: id.clone(),
                r#box: label("atelier.box"),
                repo: label("atelier.repo"),
                cwd: work.display().to_string(),
                created_ms: now_ms(),
                labels: spec.labels.clone(),
                image: spec.image.clone().unwrap_or_default(),
                env: spec.env.clone(),
                ..Default::default()
            };
            sandbox.set_state("Running");
            // The record last: a sandbox exists once it has one.
            self.save(&sandbox)?;
            Ok(sandbox.summary())
        })();
        if made.is_err() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        made
    }

    fn get(&self, id: &str) -> Result<Option<provider::Sandbox>> {
        self.before("get")?;
        anyhow::ensure!(valid_id(id), "not a sandbox id: {id:?}");
        if !self.root.join("sandboxes").join(id).join("state.json").is_file() {
            return Ok(None);
        }
        Ok(Some(Local::get(self, id)?.summary()))
    }

    fn list(&self, labels: &Labels) -> Result<Vec<provider::Sandbox>> {
        self.before("list")?;
        Ok(Local::list(self).iter().filter(|s| labels.iter().all(|(k, v)| s.labels.get(k) == Some(v))).map(Sandbox::summary).collect())
    }

    fn exec(&self, id: &str, argv: &[String], stdin: &[u8]) -> Result<ExecResult> {
        self.before("exec")?;
        let (code, stdout, stderr) = Local::exec(self, id, argv, stdin)?;
        Ok(ExecResult { code, stdout, stderr })
    }

    /// Writes a file on the sandbox's disk: owner-only from the start, then
    /// with `mode` (Unix).
    fn put_file(&self, id: &str, path: &str, bytes: &[u8], mode: u32) -> Result<()> {
        self.before("put_file")?;
        let file = self.guest_file(id, path, true)?;
        super::guest::write_with_mode(&file, bytes, mode).with_context(|| format!("cannot write {path:?} in sandbox {id}"))
    }

    /// Reads a file of the sandbox's disk; one larger than `max` is an error.
    fn get_file(&self, id: &str, path: &str, max: u64) -> Result<Vec<u8>> {
        use std::io::Read;
        self.before("get_file")?;
        let file = self.guest_file(id, path, false)?;
        let mut bytes = Vec::new();
        std::fs::File::open(&file).with_context(|| format!("cannot read {path:?} in sandbox {id}"))?.take(max.saturating_add(1)).read_to_end(&mut bytes)?;
        anyhow::ensure!(bytes.len() as u64 <= max, "{path:?} in sandbox {id} is larger than {max} bytes");
        Ok(bytes)
    }

    /// Freezes every process of the sandbox; a suspended sandbox stays as it is.
    fn suspend(&self, id: &str) -> Result<()> {
        self.before("suspend")?;
        let dir = self.dir(id)?;
        let _lock = lock(&dir.join("lock"))?;
        let mut sandbox = Local::get(self, id)?;
        match sandbox.state() {
            "Suspended" => return Ok(()),
            "Running" => {}
            other => bail!("sandbox {id} is {}", other.to_ascii_lowercase()),
        }
        let tops = tops(&refresh(&dir)?);
        for (index, top) in tops.iter().enumerate().rev() {
            if let Err(error) = canvas_sys::process::suspend_tree(top.pid, top.start_ms) {
                // Thaw what was frozen: a sandbox is suspended whole or not at all.
                for top in &tops[index..] {
                    let _ = canvas_sys::process::resume_tree(top.pid, top.start_ms);
                }
                return Err(error).with_context(|| format!("cannot freeze process {} of sandbox {id}", top.pid));
            }
        }
        sandbox.set_state("Suspended");
        self.save(&sandbox)
    }

    /// Thaws every process of the sandbox; a running sandbox stays as it is.
    fn resume(&self, id: &str) -> Result<()> {
        self.before("resume")?;
        let dir = self.dir(id)?;
        let _lock = lock(&dir.join("lock"))?;
        let mut sandbox = Local::get(self, id)?;
        match sandbox.state() {
            "Running" => return Ok(()),
            "Suspended" => {}
            other => bail!("sandbox {id} is {}", other.to_ascii_lowercase()),
        }
        for top in tops(&refresh(&dir)?) {
            canvas_sys::process::resume_tree(top.pid, top.start_ms).with_context(|| format!("cannot thaw process {} of sandbox {id}", top.pid))?;
        }
        sandbox.set_state("Running");
        self.save(&sandbox)
    }

    /// Ends the sandbox's processes and removes its folder; a sandbox that
    /// is not there is deleted already.
    fn delete(&self, id: &str) -> Result<()> {
        self.before("delete")?;
        anyhow::ensure!(valid_id(id), "not a sandbox id: {id:?}");
        let dir = self.root.join("sandboxes").join(id);
        if dir.join("state.json").is_file() {
            return Local::delete(self, id);
        }
        match std::fs::remove_dir_all(&dir) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        }
    }

    fn set_egress(&self, id: &str, policy: &EgressPolicy) -> Result<()> {
        self.before("set_egress")?;
        let dir = self.dir(id)?;
        crate::paths::write_atomic(&dir.join("egress.json"), &serde_json::to_vec_pretty(policy)?)?;
        Ok(())
    }

    fn egress_decisions(&self, id: &str) -> Result<Vec<EgressDecision>> {
        self.before("egress_decisions")?;
        match std::fs::read_to_string(self.dir(id)?.join("egress-decisions.jsonl")) {
            Ok(text) => Ok(text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    }

    /// An image of `base` when it is an absolute path to a local folder (a
    /// copy of it), else an empty one (the stock image).
    fn image_build(&self, base: &str, name: &str) -> Result<ImageRef> {
        self.before("image_build")?;
        let folder = Path::new(base);
        self.new_image(name, (folder.is_absolute() && folder.is_dir()).then_some(folder))
    }

    /// An image of the sandbox's workspace (`home/work/`).
    fn image_commit(&self, id: &str, name: &str) -> Result<ImageRef> {
        self.before("image_commit")?;
        let work = workspace(&self.dir(id)?);
        self.new_image(name, Some(&work))
    }

    fn image_delete(&self, image: &ImageRef) -> Result<()> {
        self.before("image_delete")?;
        anyhow::ensure!(plain_name(&image.id), "not an image id: {:?}", image.id);
        let images = self.root.join("images");
        for result in [std::fs::remove_dir_all(images.join(&image.id)), std::fs::remove_file(images.join(format!("{}.json", image.id)))] {
            if let Err(error) = result
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return Err(error.into());
            }
        }
        Ok(())
    }

    /// The image folders, oldest first (a folder without its `<id>.json`, made
    /// by hand, is listed by its name).
    fn images(&self) -> Result<Vec<ImageRef>> {
        self.before("images")?;
        let images = self.root.join("images");
        let Ok(entries) = std::fs::read_dir(&images) else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let id = entry.file_name().to_string_lossy().into_owned();
            if !plain_name(&id) || !entry.path().is_dir() {
                continue;
            }
            let image = std::fs::read(images.join(format!("{id}.json"))).ok().and_then(|b| serde_json::from_slice::<ImageRef>(&b).ok()).filter(|image| image.id == id);
            let created_ms = || entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as u64);
            out.push(image.unwrap_or_else(|| ImageRef { id: id.clone(), name: id.clone(), created_ms: created_ms() }));
        }
        out.sort_by(|a, b| (a.created_ms, &a.id).cmp(&(b.created_ms, &b.id)));
        Ok(out)
    }

    /// This build as the sandbox's satellite: a process of the sandbox
    /// (suspend freezes it, delete ends it), its output in
    /// `home/.atelier/satellite.log`; one that runs already is kept.
    fn start_satellite(&self, id: &str) -> Result<()> {
        self.before("start_satellite")?;
        let home = self.dir(id)?.join("home");
        if super::satellite::running(&home) {
            return Ok(());
        }
        let log = std::fs::OpenOptions::new().create(true).append(true).open(home.join(".atelier").join("satellite.log"))?;
        let mut cmd = self.command(id, &["atelier".into(), "satellite".into()], None, &[])?;
        cmd.stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
        canvas_sys::spawn::CommandExt::no_window(&mut cmd);
        let mut child = self.spawn(id, &mut cmd)?;
        std::thread::Builder::new().name("local-satellite".into()).spawn(move || {
            let _ = child.wait();
        })?;
        // Started once it holds its lock (or it ended at once: its log says why).
        let deadline = Instant::now() + Duration::from_secs(10);
        while !super::satellite::running(&home) {
            anyhow::ensure!(Instant::now() < deadline, "the satellite of {id} did not start (home/.atelier/satellite.log)");
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }

    /// Stores the key in `<root>/secrets/<name>.json` (owner-only), where
    /// only the egress stand-in reads it ([`Local::secret`]).
    fn secret_set(&self, name: &str, key: &str, value: &Secret) -> Result<()> {
        self.before("secret_set")?;
        anyhow::ensure!(plain_name(name), "not a secret name: {name:?}");
        anyhow::ensure!(!key.is_empty() && key.len() <= 256 && !key.chars().any(char::is_control), "not a secret key: {key:?}");
        let text = std::str::from_utf8(value.expose()).map_err(|_| anyhow!("the value of secret {name} is not text"))?;
        let dir = self.root.join("secrets");
        canvas_sys::fs::private_dir(&dir)?;
        let _lock = lock(&dir.join(".lock"))?;
        let path = dir.join(format!("{name}.json"));
        let mut values: serde_json::Map<String, Value> = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(Secret::new(bytes).expose()).context("a secrets file that does not parse")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
            Err(error) => return Err(error.into()),
        };
        values.insert(key.to_owned(), Value::String(text.to_owned()));
        let bytes = Secret::new(serde_json::to_vec(&values)?);
        canvas_sys::fs::write_private(&path, bytes.expose()).with_context(|| format!("cannot write secret {name}"))
    }
}

/// `atelier vm-hook-standin HOOK`: answers one hook of the pack named by
/// `ATELIER_VM_PACK` as a real hook would, with `vm::local` sandboxes
/// (`local.root` of the pack). Reads the request on standard input, prints
/// the answer.
pub fn standin(hook: &str) -> Result<()> {
    use std::io::Read;
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let request: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    let answer = match standin_local().and_then(|local| standin_answer(&local, hook, &request)) {
        Ok(answer) => answer,
        Err(error) => json!({ "ok": false, "error": format!("{error:#}") }),
    };
    println!("{answer}");
    Ok(())
}

fn standin_local() -> Result<Local> {
    let folder = canvas_common::env::var_os("ATELIER_VM_PACK").context("the stand-in needs ATELIER_VM_PACK")?;
    let (pack, _) = super::pack::load(Path::new(&folder));
    Ok(Local::new(pack.local_root.context("the stand-in needs local.root in vm.yaml")?))
}

fn standin_answer(local: &Local, hook: &str, request: &Value) -> Result<Value> {
    let s = |key: &str| request[key].as_str().unwrap_or_default().to_owned();
    let strings = |value: &Value| -> Vec<String> { value.as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default() };
    Ok(match hook {
        "claim" => {
            let sandbox = local.claim(&s("box"), &s("repo"), &s("url"), &s("branch"), &s("base"))?;
            json!({ "ok": true, "sandbox": sandbox.id, "cwd": sandbox.cwd })
        }
        "shell-command" => {
            let id = s("sandbox");
            local.get(&id)?;
            let exe = std::env::current_exe()?.display().to_string();
            let mut argv = vec![exe, "vm-hook-standin".into(), "run".into(), "--sandbox".into(), id];
            if let Some(env) = request["env"].as_object() {
                for (k, v) in env {
                    argv.extend(["--env".into(), format!("{k}={}", v.as_str().unwrap_or_default())]);
                }
            }
            argv.push("--".into());
            argv.extend(strings(&request["argv"]));
            json!({ "ok": true, "argv": argv })
        }
        "exec" => {
            use base64::Engine as _;
            let b64 = base64::engine::general_purpose::STANDARD;
            let stdin = b64.decode(request["stdin"].as_str().unwrap_or_default()).unwrap_or_default();
            let (code, stdout, stderr) = local.exec(&s("sandbox"), &strings(&request["argv"]), &stdin)?;
            json!({ "ok": true, "code": code, "stdout": b64.encode(stdout), "stderr": crate::vm::hooks::first_line(&stderr) })
        }
        "suspend" => {
            Provider::suspend(local, &s("sandbox"))?;
            json!({ "ok": true })
        }
        "resume" => {
            Provider::resume(local, &s("sandbox"))?;
            json!({ "ok": true })
        }
        "delete" => {
            local.delete(&s("sandbox"))?;
            json!({ "ok": true })
        }
        "image" => json!({ "ok": true, "image": format!("local-{}", s("repo")) }),
        other => bail!("the stand-in has no {other} hook"),
    })
}

/// `atelier vm-hook-standin run --sandbox ID [--env K=V]… -- PROGRAM ARGS…`:
/// the "shell into the guest" the stand-in's `shell-command` answers: runs
/// the program in the sandbox with this terminal (exits with its code). This
/// process is recorded as the sandbox's, so suspend freezes the program and
/// delete ends it.
pub fn standin_run(id: &str, env: &[String], argv: &[String]) -> Result<()> {
    let local = standin_local()?;
    let env: Vec<(String, String)> = env.iter().filter_map(|e| e.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned()))).collect();
    let mut cmd = local.command(id, argv, None, &env)?;
    let _ = local.track(id, std::process::id());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = cmd.exec();
        bail!("cannot start {}: {error}", argv[0]);
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status().with_context(|| format!("cannot start {}", argv[0]))?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Read, Write};
    use std::process::Stdio;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    /// A provider on a new temporary root, without the environment's knobs.
    fn fresh() -> (PathBuf, Local) {
        let root = std::env::temp_dir().join(format!("atelier-local-{}", uuid::Uuid::new_v4()));
        (root.clone(), Local::with_faults(root, "", 0).unwrap())
    }

    /// This test binary as a process of a sandbox (the same on every OS):
    /// `ticker` prints `tick N` every 50 ms for at most a minute; `parent`
    /// runs a ticker as its child; `echo` prints its standard input, its
    /// folder and its environment.
    fn helper(mode: &str) -> Vec<String> {
        let exe = std::env::current_exe().unwrap().display().to_string();
        [exe.as_str(), "vm::local::tests::helper_process", "--exact", "--ignored", "--nocapture", "--test-threads=1", &format!("helper:{mode}")].map(str::to_owned).to_vec()
    }

    #[test]
    #[ignore = "a helper process the sandbox tests run"]
    fn helper_process() {
        let Some(mode) = std::env::args().find_map(|a| a.strip_prefix("helper:").map(str::to_owned)) else { return };
        let mut out = std::io::stdout().lock();
        match mode.as_str() {
            "parent" => {
                drop(out);
                let argv = helper("ticker");
                let _ = Command::new(&argv[0]).args(&argv[1..]).status();
            }
            "echo" => {
                let mut stdin = String::new();
                let _ = std::io::stdin().read_to_string(&mut stdin);
                let _ = writeln!(out, "stdin={stdin}");
                let _ = writeln!(out, "cwd={}", std::env::current_dir().unwrap().display());
                for (k, v) in std::env::vars_os() {
                    let _ = writeln!(out, "env:{}={}", k.to_string_lossy(), v.to_string_lossy());
                }
            }
            _ => {
                drop(out);
                let deadline = Instant::now() + Duration::from_secs(60);
                let mut tick = 0u64;
                while Instant::now() < deadline {
                    let mut out = std::io::stdout().lock();
                    let _ = writeln!(out, "tick {tick}").and_then(|_| out.flush());
                    drop(out);
                    tick += 1;
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    /// Counts the `tick` lines `child` prints.
    fn ticks(child: &mut Child) -> Arc<AtomicUsize> {
        let count = Arc::new(AtomicUsize::new(0));
        let stdout = child.stdout.take().unwrap();
        let seen = count.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                if line.starts_with("tick") {
                    seen.fetch_add(1, SeqCst);
                }
            }
        });
        count
    }

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A sandbox process printing ticks (a `parent` and its ticker child).
    fn ticking(local: &Local, id: &str) -> (Child, Arc<AtomicUsize>) {
        let mut cmd = local.command(id, &helper("parent"), None, &[]).unwrap();
        cmd.stdout(Stdio::piped()).stdin(Stdio::null());
        let mut child = local.spawn(id, &mut cmd).unwrap();
        let count = ticks(&mut child);
        wait_for("the ticker did not start", || count.load(SeqCst) >= 3);
        (child, count)
    }

    /// Asserts no tick arrives for a while (after those already in the pipe).
    fn silent(count: &AtomicUsize) -> usize {
        std::thread::sleep(Duration::from_millis(300));
        let at = count.load(SeqCst);
        std::thread::sleep(Duration::from_millis(700));
        assert_eq!(count.load(SeqCst), at, "no output while frozen");
        at
    }

    fn labels(pairs: &[(&str, &str)]) -> Labels {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn files(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                out.extend(files(&entry.path()));
            } else if kind.is_file() {
                out.push(entry.path());
            }
        }
        out
    }

    /// A symbolic link (`false` where this user may not make one: Windows without developer mode).
    fn link(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(target, link).is_ok();
        #[cfg(windows)]
        return if target.is_dir() { std::os::windows::fs::symlink_dir(target, link).is_ok() } else { std::os::windows::fs::symlink_file(target, link).is_ok() };
    }

    #[test]
    fn sandboxes_are_folders_with_a_home_of_their_own() {
        let root = std::env::temp_dir().join(format!("atelier-local-{}", uuid::Uuid::new_v4()));
        let local = Local::new(root.clone());
        let sandbox = local.claim("box-1", "contoso-app", "https://example.com/app.git", "atelier/x", "main").unwrap();
        assert!(valid_id(&sandbox.id) && Path::new(&sandbox.cwd).is_dir());
        assert_eq!(local.list(), vec![sandbox.clone()]);
        let env = local.env(&sandbox.id).unwrap();
        let home = root.join("sandboxes").join(&sandbox.id).join("home");
        for key in ["HOME", "USERPROFILE", "ATELIER_HOME", "COPILOT_HOME", "CLAUDE_CONFIG_DIR"] {
            let value = &env.iter().find(|(k, _)| k == key).unwrap().1;
            assert!(Path::new(value).starts_with(&home), "{key} = {value}");
        }
        assert!(local.claim("b", "../x", "", "b", "main").is_err(), "a repository key is one plain name");
        assert!(local.get("sbx-../../etc").is_err() && local.get("sbx-000000000000").is_err());
        local.set_suspended(&sandbox.id, true).unwrap();
        assert!(local.get(&sandbox.id).unwrap().suspended);
        assert_eq!(local.get(&sandbox.id).unwrap().state(), "Suspended");
        let provider: &dyn Provider = &local;
        assert_eq!(provider.list(&labels(&[("atelier.box", "box-1")])).unwrap(), vec![provider::Sandbox { id: sandbox.id.clone(), state: "Suspended".into(), labels: labels(&[("atelier.box", "box-1"), ("atelier.repo", "contoso-app")]) }]);
        local.delete(&sandbox.id).unwrap();
        assert!(local.list().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sandboxes_are_made_listed_by_labels_and_deleted() {
        let (root, local) = fresh();
        let provider: &dyn Provider = &local;
        assert_eq!(provider.name(), "local");
        let spec = |pairs: &[(&str, &str)]| SandboxSpec { labels: labels(pairs), cpu_milli: 1000, memory_mi: 2048, disk_gi: 20, ..Default::default() };
        let pool = provider.create(&spec(&[("atelier.hub", "h1"), ("atelier.role", "pool")])).unwrap();
        let session = provider.create(&spec(&[("atelier.hub", "h1"), ("atelier.role", "session"), ("atelier.box", "box-1"), ("atelier.repo", "contoso-app")])).unwrap();
        let other = provider.create(&spec(&[("atelier.hub", "h2")])).unwrap();
        assert!(valid_id(&pool.id) && pool.running() && pool.labels["atelier.role"] == "pool");
        let ids = |sandboxes: Vec<provider::Sandbox>| sandboxes.into_iter().map(|s| s.id).collect::<Vec<_>>();
        let mut h1 = vec![pool.id.clone(), session.id.clone()];
        h1.sort();
        assert_eq!(ids(provider.list(&labels(&[("atelier.hub", "h1")])).unwrap()), h1);
        assert_eq!(ids(provider.list(&labels(&[("atelier.hub", "h1"), ("atelier.role", "session")])).unwrap()), vec![session.id.clone()]);
        assert_eq!(provider.list(&Labels::new()).unwrap().len(), 3);
        assert!(provider.list(&labels(&[("atelier.hub", "h3")])).unwrap().is_empty());
        assert_eq!(provider.get(&session.id).unwrap(), Some(session.clone()));
        let record = Local::get(&local, &session.id).unwrap();
        assert_eq!((record.r#box.as_str(), record.repo.as_str()), ("box-1", "contoso-app"));
        assert!(provider.get("sbx-000000000000").unwrap().is_none());
        assert!(provider.get("../sandboxes").is_err(), "not an id");

        provider.delete(&other.id).unwrap();
        assert!(provider.get(&other.id).unwrap().is_none() && !root.join("sandboxes").join(&other.id).exists());
        provider.delete(&other.id).unwrap();
        assert!(provider.exec(&other.id, &helper("echo"), b"").is_err(), "a deleted sandbox runs nothing");
        for sandbox in [pool, session] {
            provider.delete(&sandbox.id).unwrap();
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn exec_runs_in_the_sandbox_with_its_environment_and_input() {
        let (root, local) = fresh();
        let provider: &dyn Provider = &local;
        let spec = SandboxSpec { env: vec![("GREETING".into(), "hello".into()), ("HOME".into(), "/elsewhere".into())], ..Default::default() };
        let sandbox = provider.create(&spec).unwrap();
        let out = provider.exec(&sandbox.id, &helper("echo"), b"some input").unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert_eq!(out.code, 0, "{text}{}", out.stderr);
        assert!(text.contains("stdin=some input"), "{text}");
        assert!(text.contains("env:GREETING=hello"), "the sandbox's plain environment: {text}");
        let dir = root.join("sandboxes").join(&sandbox.id).canonicalize().unwrap();
        let line = |prefix: &str| text.lines().find_map(|l| l.strip_prefix(prefix)).map(|v| PathBuf::from(v.trim_end()).canonicalize().unwrap()).unwrap();
        assert_eq!(line("cwd="), dir.join("home").join("work"));
        assert_eq!(line("env:HOME="), dir.join("home"), "the sandbox's home wins over its plain environment");
        assert!(local.processes(&sandbox.id).unwrap().is_empty(), "a command run to its end leaves nothing running");
        let none = provider.exec(&sandbox.id, &["atelier-no-such-program".to_owned()], b"");
        assert!(none.is_err());
        provider.delete(&sandbox.id).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn suspend_freezes_every_process_of_the_sandbox_and_resume_thaws_them() {
        let (root, local) = fresh();
        let provider: &dyn Provider = &local;
        let sandbox = provider.create(&SandboxSpec::default()).unwrap();
        let (mut child, count) = ticking(&local, &sandbox.id);
        let procs = local.processes(&sandbox.id).unwrap();
        assert!(procs.len() >= 2 && procs[0].pid == child.id(), "the parent and its ticker are recorded: {procs:?}");

        provider.suspend(&sandbox.id).unwrap();
        assert!(provider.get(&sandbox.id).unwrap().unwrap().suspended());
        let frozen_at = silent(&count);
        assert!(procs.iter().all(Proc::alive), "frozen, not ended");
        assert!(provider.exec(&sandbox.id, &helper("echo"), b"").unwrap_err().to_string().contains("cannot run"), "nothing starts in a suspended sandbox");
        // A second suspend does nothing (Windows counts suspensions: one resume must thaw it).
        provider.suspend(&sandbox.id).unwrap();

        provider.resume(&sandbox.id).unwrap();
        assert!(provider.get(&sandbox.id).unwrap().unwrap().running());
        wait_for("no output after resume", || count.load(SeqCst) > frozen_at + 2);
        provider.resume(&sandbox.id).unwrap();
        let at = count.load(SeqCst);
        wait_for("a second resume stopped the output", || count.load(SeqCst) > at + 2);

        // Frozen, then deleted: no process and no folder are left.
        provider.suspend(&sandbox.id).unwrap();
        provider.delete(&sandbox.id).unwrap();
        child.wait().unwrap();
        assert!(!procs.iter().any(Proc::alive), "delete leaves no process");
        assert!(!root.join("sandboxes").join(&sandbox.id).exists(), "nor its folder");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_process_that_leaves_its_parent_stays_the_sandboxs() {
        let (root, local) = fresh();
        let provider: &dyn Provider = &local;
        let sandbox = provider.create(&SandboxSpec::default()).unwrap();
        let (mut parent, count) = ticking(&local, &sandbox.id);
        let procs = local.processes(&sandbox.id).unwrap();
        // The parent ends alone; its ticker goes on without it.
        parent.kill().unwrap();
        parent.wait().unwrap();
        let at = count.load(SeqCst);
        wait_for("the orphaned ticker goes on", || count.load(SeqCst) > at + 2);
        provider.suspend(&sandbox.id).unwrap();
        silent(&count);
        provider.resume(&sandbox.id).unwrap();
        provider.delete(&sandbox.id).unwrap();
        assert!(!procs.iter().any(Proc::alive), "the orphan is ended with the sandbox: {procs:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_recycled_pid_is_never_signalled() {
        let (root, local) = fresh();
        let provider: &dyn Provider = &local;
        let sandbox = provider.create(&SandboxSpec::default()).unwrap();
        // A process that is not the sandbox's, at a recorded pid whose start
        // time differs: what a pid reused after the sandbox's process ended looks like.
        let argv = helper("ticker");
        let mut stranger = Command::new(&argv[0]).args(&argv[1..]).stdout(Stdio::piped()).spawn().unwrap();
        let count = ticks(&mut stranger);
        wait_for("the stranger did not start", || count.load(SeqCst) >= 2);
        let started = canvas_sys::process::process_start_ms(stranger.id()).unwrap();
        let dir = root.join("sandboxes").join(&sandbox.id);
        std::fs::write(dir.join("processes.json"), serde_json::to_vec(&[Proc { pid: stranger.id(), start_ms: started - 1_000 }]).unwrap()).unwrap();

        provider.suspend(&sandbox.id).unwrap();
        let at = count.load(SeqCst);
        wait_for("the stranger was frozen", || count.load(SeqCst) > at + 2);
        assert!(read_processes(&dir).is_empty(), "the stale record is dropped");
        provider.resume(&sandbox.id).unwrap();
        std::fs::write(dir.join("processes.json"), serde_json::to_vec(&[Proc { pid: stranger.id(), start_ms: started + 1_000 }]).unwrap()).unwrap();
        provider.delete(&sandbox.id).unwrap();
        assert_eq!(canvas_sys::process::process_start_ms(stranger.id()), Some(started), "nor ended with the sandbox");
        stranger.kill().unwrap();
        stranger.wait().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn files_stay_on_the_sandbox_disk() {
        let (root, local) = fresh();
        let provider: &dyn Provider = &local;
        let sandbox = provider.create(&SandboxSpec::default()).unwrap();
        let id = sandbox.id.as_str();
        let dir = root.join("sandboxes").join(id);
        let home = dir.join("home");
        // Relative paths are from the guest's home; files are owner-only unless asked.
        provider.put_file(id, ".atelier/satellite.json", b"{}", 0o600).unwrap();
        assert_eq!(provider.get_file(id, ".atelier/satellite.json", 2).unwrap(), b"{}");
        provider.put_file(id, ".local/bin/tool", b"#!/bin/sh\n", 0o755).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &str| std::fs::metadata(home.join(p)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(".atelier/satellite.json"), 0o600, "owner-only");
            assert_eq!(mode(".local/bin/tool"), 0o755, "a program");
        }
        provider.put_file(id, "work/a/b/c.txt", b"deep", 0o600).unwrap();
        let inside = home.join("work").join("a").join("b").join("c.txt");
        assert_eq!(std::fs::read(&inside).unwrap(), b"deep", "the folders on the way are made");
        assert_eq!(provider.get_file(id, &inside.display().to_string(), 100).unwrap(), b"deep", "an absolute path inside the sandbox's home");
        assert!(provider.get_file(id, "work/a/b/c.txt", 3).unwrap_err().to_string().contains("larger than 3 bytes"));
        assert!(provider.get_file(id, "work/missing.txt", 100).is_err());

        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("host.txt"), b"host").unwrap();
        let absolute = |p: &Path| p.display().to_string();
        let refused = [absolute(&outside.join("x.txt")), absolute(&dir.join("state.json")), absolute(&dir.join("egress.json")), absolute(&home), absolute(&dir.join("x.txt"))];
        for path in ["../x.txt", "work/../../x.txt", "work/../../../outside/x.txt", "", "."].into_iter().chain(refused.iter().map(String::as_str)) {
            assert!(provider.put_file(id, path, b"x", 0o600).is_err(), "put {path:?}");
            assert!(provider.get_file(id, path, 100).is_err(), "get {path:?}");
        }
        assert!(provider.get_file(id, &outside.join("host.txt").display().to_string(), 100).is_err());
        assert!(!outside.join("x.txt").exists() && !root.join("x.txt").exists() && !dir.join("x.txt").exists());

        // Links leading out are refused, as folders on the way and as the file itself.
        if link(&outside, &home.join("work").join("out")) {
            assert!(provider.put_file(id, "work/out/x.txt", b"x", 0o600).is_err());
            assert!(provider.put_file(id, "work/out/new/x.txt", b"x", 0o600).is_err());
            assert!(provider.get_file(id, "work/out/host.txt", 100).is_err());
            assert!(link(&outside.join("host.txt"), &home.join("work").join("leak.txt")));
            assert!(provider.get_file(id, "work/leak.txt", 100).is_err());
            assert!(provider.put_file(id, "work/leak.txt", b"overwritten", 0o600).is_err());
            assert!(!outside.join("x.txt").exists() && !outside.join("new").exists());
            assert_eq!(std::fs::read(outside.join("host.txt")).unwrap(), b"host");
            // A link that stays inside is followed.
            assert!(link(&home.join("work").join("a"), &home.join("work").join("inner")));
            provider.put_file(id, "work/inner/b/d.txt", b"inside", 0o600).unwrap();
            assert_eq!(std::fs::read(home.join("work/a/b/d.txt")).unwrap(), b"inside");
        }
        provider.delete(id).unwrap();
        assert!(outside.join("host.txt").is_file(), "delete stays in the sandbox");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn secrets_stay_out_of_sandboxes_and_their_processes() {
        let (root, local) = fresh();
        let provider: &dyn Provider = &local;
        let sandbox = provider.create(&SandboxSpec::default()).unwrap();
        let value = format!("s3cret-{}", uuid::Uuid::new_v4().simple());
        provider.secret_set("forge", "token", &Secret::new(value.clone().into_bytes())).unwrap();
        provider.secret_set("forge", "user", &Secret::new(b"atelier".to_vec())).unwrap();
        let file = root.join("secrets").join("forge.json");
        assert!(file.is_file() && !file.starts_with(root.join("sandboxes")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600, "owner-only");
        }
        assert_eq!(local.secret("forge", "token").unwrap().unwrap().expose(), value.as_bytes(), "a second key keeps the first");
        assert_eq!(local.secret("forge", "user").unwrap().unwrap().expose(), b"atelier");
        assert!(local.secret("forge", "other").unwrap().is_none() && local.secret("nope", "token").unwrap().is_none());
        for name in ["", "../x", "a/b", "a\\b", ".hidden", "sandboxes/x"] {
            assert!(provider.secret_set(name, "token", &Secret::new(b"x".to_vec())).is_err(), "{name:?}");
        }
        assert!(provider.secret_set("forge", "", &Secret::new(b"x".to_vec())).is_err());

        let out = provider.exec(&sandbox.id, &helper("echo"), b"").unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("env:HOME=") && !text.contains(&value), "a process of the sandbox never sees the value");
        for path in files(&root.join("sandboxes")) {
            assert!(!String::from_utf8_lossy(&std::fs::read(&path).unwrap()).contains(&value), "{} holds the value", path.display());
        }
        provider.delete(&sandbox.id).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn images_and_egress_are_files_of_the_provider() {
        let (root, local) = fresh();
        let provider: &dyn Provider = &local;
        let policy = EgressPolicy {
            default_action: "Deny".into(),
            rules: vec![provider::EgressRule { name: "forge".into(), host: "example.com".into(), methods: vec!["GET".into()], action: "Allow".into(), ..Default::default() }],
        };
        let sandbox = provider.create(&SandboxSpec { egress: Some(policy.clone()), ..Default::default() }).unwrap();
        assert_eq!(local.egress(&sandbox.id).unwrap(), Some(policy.clone()));
        let open = EgressPolicy { default_action: "Allow".into(), rules: Vec::new() };
        provider.set_egress(&sandbox.id, &open).unwrap();
        assert_eq!(local.egress(&sandbox.id).unwrap(), Some(open));
        let policy_file = root.join("sandboxes").join(&sandbox.id).join("egress.json");
        assert!(provider.put_file(&sandbox.id, &policy_file.display().to_string(), b"{}", 0o600).is_err(), "the guest cannot change its policy");
        assert!(provider.egress_decisions(&sandbox.id).unwrap().is_empty());
        let refused = EgressDecision { ms: 1, host: "contoso.com".into(), method: "POST".into(), path: "/x".into(), allowed: false };
        let allowed = EgressDecision { ms: 2, host: "example.com".into(), method: "GET".into(), path: "/".into(), allowed: true };
        local.log_egress(&sandbox.id, &refused).unwrap();
        std::fs::OpenOptions::new().append(true).open(root.join("sandboxes").join(&sandbox.id).join("egress-decisions.jsonl")).unwrap().write_all(b"not json\n").unwrap();
        local.log_egress(&sandbox.id, &allowed).unwrap();
        assert_eq!(provider.egress_decisions(&sandbox.id).unwrap(), vec![refused, allowed]);

        let base = root.join("base");
        std::fs::create_dir_all(base.join("docs")).unwrap();
        std::fs::write(base.join("docs").join("README.md"), b"seeded").unwrap();
        let seeded = provider.image_build(&base.display().to_string(), "seeded").unwrap();
        let stock = provider.image_build("ubuntu:24.04", "stock").unwrap();
        assert!(seeded.id.starts_with("img-") && seeded.name == "seeded" && seeded.created_ms > 0);
        let made = provider.create(&SandboxSpec { image: Some(seeded.id.clone()), ..Default::default() }).unwrap();
        let work = root.join("sandboxes").join(&made.id).join("home").join("work");
        assert_eq!(std::fs::read(work.join("docs").join("README.md")).unwrap(), b"seeded", "an image is copied into the workspace (~/work)");
        provider.put_file(&made.id, "work/NOTES.md", b"notes", 0o644).unwrap();
        let committed = provider.image_commit(&made.id, "after").unwrap();
        let folder = root.join("images").join(&committed.id);
        assert!(folder.join("docs").join("README.md").is_file() && std::fs::read(folder.join("NOTES.md")).unwrap() == b"notes");
        let by_hand = root.join("images").join("local-contoso-app");
        std::fs::create_dir_all(&by_hand).unwrap();
        let names = |images: Vec<ImageRef>| images.into_iter().map(|i| i.name).collect::<HashSet<_>>();
        assert_eq!(names(provider.images().unwrap()), HashSet::from(["seeded".to_owned(), "stock".to_owned(), "after".to_owned(), "local-contoso-app".to_owned()]));
        assert!(provider.create(&SandboxSpec { image: Some("local-contoso-app".into()), ..Default::default() }).unwrap().running(), "a folder made by hand is an image too");
        provider.image_delete(&stock).unwrap();
        provider.image_delete(&stock).unwrap();
        assert!(!names(provider.images().unwrap()).contains("stock") && !root.join("images").join(&stock.id).exists());
        for image in ["img-000000000000", "../base", ""] {
            assert!(provider.create(&SandboxSpec { image: Some(image.into()), ..Default::default() }).is_err(), "{image:?}");
        }
        assert!(provider.image_delete(&ImageRef { id: "../base".into(), ..Default::default() }).is_err());
        assert!(base.join("docs").join("README.md").is_file(), "an image's source is left alone");
        assert_eq!(provider.list(&Labels::new()).unwrap().len(), 3, "a failed create leaves no sandbox");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_failure_knobs_fail_the_nth_call_and_slow_every_call() {
        let (root, _) = fresh();
        let local = Local::with_faults(root.clone(), "create:2, resume:1", 0).unwrap();
        let provider: &dyn Provider = &local;
        let first = provider.create(&SandboxSpec::default()).unwrap();
        let error = provider.create(&SandboxSpec::default()).unwrap_err().to_string();
        assert!(error.contains("injected failure") && error.contains("create call 2"), "{error}");
        provider.create(&SandboxSpec::default()).unwrap();
        assert_eq!(provider.list(&Labels::new()).unwrap().len(), 2);
        provider.suspend(&first.id).unwrap();
        assert!(provider.resume(&first.id).is_err());
        assert!(provider.get(&first.id).unwrap().unwrap().suspended(), "a failed resume changes nothing");
        provider.resume(&first.id).unwrap();
        // The count is the root's, so another provider on it (a hook's process) goes on counting.
        let again = Local::with_faults(root.clone(), "create:4", 0).unwrap();
        assert!(Provider::create(&again, &SandboxSpec::default()).is_err());
        assert!(Provider::create(&again, &SandboxSpec::default()).is_ok());

        let slow = Local::with_faults(root.clone(), "", 150).unwrap();
        let start = Instant::now();
        Provider::images(&slow).unwrap();
        assert!(start.elapsed() >= Duration::from_millis(150));

        for knob in ["boot:1", "create", "create:0", "create:x"] {
            assert!(Local::with_faults(root.clone(), knob, 0).is_err(), "{knob:?}");
        }
        let malformed = Faults::parse("create:x", "").map_err(|e| e.to_string()).unwrap_err();
        let broken = Local { root: root.clone(), faults: Faults { malformed: Some(malformed), ..Default::default() } };
        assert!(Provider::images(&broken).unwrap_err().to_string().contains("ATELIER_VM_TEST_FAIL"), "a malformed knob fails every call");
        assert_eq!(Faults::parse("", " 25 ").unwrap(), Faults { latency_ms: 25, ..Default::default() });
        assert!(Faults::parse("", "soon").is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_stand_in_answers_the_hooks_with_these_sandboxes() {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD;
        let (root, local) = fresh();
        let claim = standin_answer(&local, "claim", &json!({ "box": "box-1", "repo": "contoso-app", "url": "https://example.com/app.git", "branch": "atelier/x", "base": "main" })).unwrap();
        let id = claim["sandbox"].as_str().unwrap().to_owned();
        assert!(claim["ok"] == true && valid_id(&id) && Path::new(claim["cwd"].as_str().unwrap()).is_dir(), "{claim}");

        let shell = standin_answer(&local, "shell-command", &json!({ "sandbox": id, "argv": ["copilot", "--resume"], "env": { "K": "V" } })).unwrap();
        let argv: Vec<&str> = shell["argv"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(argv[1..], ["vm-hook-standin", "run", "--sandbox", id.as_str(), "--env", "K=V", "--", "copilot", "--resume"]);

        let exec = standin_answer(&local, "exec", &json!({ "sandbox": id, "argv": helper("echo"), "stdin": b64.encode("from the hub") })).unwrap();
        assert_eq!(exec["code"], 0);
        assert!(String::from_utf8_lossy(&b64.decode(exec["stdout"].as_str().unwrap()).unwrap()).contains("stdin=from the hub"));

        // The suspend and resume hooks freeze and thaw the sandbox's processes; delete ends them.
        let (mut child, count) = ticking(&local, &id);
        let procs = local.processes(&id).unwrap();
        assert_eq!(standin_answer(&local, "suspend", &json!({ "sandbox": id })).unwrap()["ok"], true);
        let frozen_at = silent(&count);
        assert_eq!(standin_answer(&local, "resume", &json!({ "sandbox": id })).unwrap()["ok"], true);
        wait_for("no output after the resume hook", || count.load(SeqCst) > frozen_at + 2);
        assert_eq!(standin_answer(&local, "delete", &json!({ "sandbox": id })).unwrap()["ok"], true);
        child.wait().unwrap();
        assert!(!procs.iter().any(Proc::alive) && local.list().is_empty());
        assert!(standin_answer(&local, "delete", &json!({ "sandbox": id })).is_err(), "the hook says when there is no such sandbox");
        assert!(standin_answer(&local, "suspend", &json!({ "sandbox": "sbx-000000000000" })).is_err());
        assert_eq!(standin_answer(&local, "image", &json!({ "repo": "contoso-app" })).unwrap()["image"], "local-contoso-app");
        assert!(standin_answer(&local, "reboot", &json!({})).is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
