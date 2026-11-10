//! Where automations run on a schedule (MAC-LINUX-PLAN D7, §5.5): one
//! [`Scheduler`] per platform behind one trait. The manifests stay the source
//! of truth (`factory`); a backend renders a manifest into its own
//! definition, installs it, reads installed definitions back (drift,
//! `automations import`), runs, enables and disables tasks.
//!
//! - Windows: Task Scheduler (`taskschd` and the XML of `factory`), as before.
//! - Linux: systemd user units (a service and a timer per task) when a user
//!   manager runs, else a marked block in the user's crontab.
//! - macOS: launchd, a LaunchAgent per task.
//!
//! Off Windows every run goes through the built-in launcher
//! (`atelier automations launch <state>/tasks/<name>.json`, see
//! [`crate::launcher`]), which writes the Python launcher's log lines and
//! environment and enforces what a backend cannot (time limit, overlap,
//! retries, success codes). A folder's `automations.yaml` may still name its
//! own `launcher` and `python`. `ATELIER_SCHEDULER=launchd|systemd|cron|none`
//! picks a backend explicitly.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::Result;

pub use canvas_model::automation::TaskInfo;

use crate::factory::{Defaults, Imported, Manifest};

// The launcher-based backends build everywhere (their rendering is tested on
// every OS); `choose` picks launchd on macOS, systemd or cron on Linux.
pub mod common;
pub mod cron;
pub mod launchd;
mod none;
pub mod systemd;
#[cfg(windows)]
mod task_scheduler;

pub use none::NoScheduler;

/// A scheduler backend. Names are the manifests' (`TaskInfo::name`); lists are
/// keyed by the lower-case name. `TaskInfo::xml` holds the installed
/// definition in the backend's own format (Task Scheduler XML, unit files,
/// crontab lines, a launchd plist).
pub trait Scheduler: Send + Sync {
    /// Its name in messages ("Task Scheduler", "systemd", "cron", "launchd").
    fn name(&self) -> &'static str;

    /// The installed tasks: Task Scheduler's root folder; elsewhere the tasks
    /// Atelier installed.
    fn list(&self) -> Result<BTreeMap<String, TaskInfo>>;

    /// One task's installed definition.
    fn definition(&self, name: &str) -> Result<Option<String>> {
        Ok(self.task(name)?.map(|t| t.xml))
    }

    /// One installed task (what [`Scheduler::drift`] needs of it).
    fn task(&self, name: &str) -> Result<Option<TaskInfo>> {
        Ok(self.list()?.remove(&name.to_lowercase()))
    }

    /// Starts a run now (the schedule is unchanged).
    fn run(&self, name: &str) -> Result<()>;

    /// Enables or disables an installed task (the caller edits the manifest).
    fn set_enabled(&self, name: &str, enabled: bool) -> Result<()>;

    /// Installs `definition` (from [`Scheduler::render`]) as `name`, replacing
    /// what was there.
    fn register(&self, name: &str, definition: &str) -> Result<()>;

    /// Removes an installed task.
    fn delete(&self, name: &str) -> Result<()>;

    /// The definition of a manifest; `existing` is what is installed now
    /// (Task Scheduler keeps its principal).
    fn render(&self, m: &Manifest, d: &Defaults, existing: Option<&TaskInfo>) -> String;

    /// An installed task read back as a manifest (`automations import`).
    fn import(&self, task: &TaskInfo) -> Imported;

    /// What differs between a manifest and its installed task, one line per
    /// field (`what: installed → manifest`).
    fn drift(&self, m: &Manifest, d: &Defaults, task: &TaskInfo) -> Vec<String>;

    /// Runs read the launcher's `<state>/tasks/<name>.json`, which `apply`
    /// writes (Task Scheduler: only with the folder's Python launcher).
    fn uses_task_json(&self, d: &Defaults) -> bool {
        !d.launcher.is_empty()
    }

    /// The launcher's file for a manifest.
    fn task_json(&self, m: &Manifest, d: &Defaults) -> String {
        crate::factory::task_json(m, d)
    }

    /// [`Scheduler::drift`] already compares the launcher's file.
    fn drift_covers_task_json(&self) -> bool {
        false
    }

    /// A task `automations import` picks when none is named.
    fn importable(&self, task: &TaskInfo) -> bool;

    /// An installed task run through this folder's launcher (`apply` lists
    /// the ones without a manifest).
    fn launched_by(&self, task: &TaskInfo, d: &Defaults) -> bool;
}

/// This machine's scheduler (chosen once per process).
pub fn get() -> &'static dyn Scheduler {
    static CHOSEN: OnceLock<Box<dyn Scheduler>> = OnceLock::new();
    CHOSEN.get_or_init(choose).as_ref()
}

#[cfg(windows)]
fn choose() -> Box<dyn Scheduler> {
    Box::new(task_scheduler::TaskScheduler)
}

#[cfg(unix)]
fn choose() -> Box<dyn Scheduler> {
    match canvas_common::env::var("ATELIER_SCHEDULER").ok().as_deref().map(str::trim) {
        Some("systemd") => return Box::new(systemd::Systemd::user()),
        Some("cron") => return Box::new(cron::Cron::user()),
        Some("launchd") => return Box::new(launchd::Launchd::user()),
        Some("none") => return Box::new(NoScheduler),
        _ => {}
    }
    match canvas_sys::host::platform() {
        canvas_sys::host::Platform::Linux if systemd::Systemd::available() => Box::new(systemd::Systemd::user()),
        canvas_sys::host::Platform::Linux if cron::Cron::available() => Box::new(cron::Cron::user()),
        canvas_sys::host::Platform::MacOs if launchd::Launchd::available() => Box::new(launchd::Launchd::user()),
        _ => Box::new(NoScheduler),
    }
}

/// The state folder a launcher's task file belongs to (`<state>/tasks/x.json`).
pub fn state_of(task_json: &Path) -> Option<&Path> {
    task_json.parent().and_then(Path::parent)
}
