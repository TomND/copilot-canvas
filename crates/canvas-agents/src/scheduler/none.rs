//! No scheduler on this machine (macOS until its launchd backend; Linux
//! without a user systemd and without cron): nothing is listed and every
//! change is refused with that reason.

use std::collections::BTreeMap;

use anyhow::{Result, bail};

use super::{Scheduler, TaskInfo};
use crate::factory::{Defaults, Imported, Manifest};

const NONE: &str = "automations need a scheduler (Task Scheduler, launchd, a user systemd or cron), and none is available on this machine";

pub struct NoScheduler;

impl Scheduler for NoScheduler {
    fn name(&self) -> &'static str {
        "the scheduler"
    }

    fn list(&self) -> Result<BTreeMap<String, TaskInfo>> {
        Ok(BTreeMap::new())
    }

    fn run(&self, _name: &str) -> Result<()> {
        bail!(NONE)
    }

    fn set_enabled(&self, _name: &str, _enabled: bool) -> Result<()> {
        bail!(NONE)
    }

    fn register(&self, _name: &str, _definition: &str) -> Result<()> {
        bail!(NONE)
    }

    fn delete(&self, _name: &str) -> Result<()> {
        bail!(NONE)
    }

    fn render(&self, _m: &Manifest, _d: &Defaults, _existing: Option<&TaskInfo>) -> String {
        String::new()
    }

    fn import(&self, task: &TaskInfo) -> Imported {
        let mut out = Imported::default();
        out.manifest.name = task.name.clone();
        out
    }

    fn drift(&self, _m: &Manifest, _d: &Defaults, _task: &TaskInfo) -> Vec<String> {
        Vec::new()
    }

    fn importable(&self, _task: &TaskInfo) -> bool {
        false
    }

    fn launched_by(&self, _task: &TaskInfo, _d: &Defaults) -> bool {
        false
    }
}
