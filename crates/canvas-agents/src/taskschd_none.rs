//! No scheduler backend yet on this platform: the Task Scheduler is
//! Windows'; launchd, systemd user timers and cron come with the
//! `Scheduler` trait (MAC-LINUX-PLAN D7, Phase 2). Listing finds nothing
//! and every change is refused with that reason.

use std::collections::BTreeMap;

use anyhow::{Result, bail};

pub use canvas_model::automation::TaskInfo;

const NOT_YET: &str = "automations need a scheduler backend (launchd, systemd or cron), which is not available on this platform yet";

pub fn list() -> Result<BTreeMap<String, TaskInfo>> {
    Ok(BTreeMap::new())
}

pub fn xml(_name: &str) -> Result<Option<String>> {
    Ok(None)
}

pub fn run(_name: &str) -> Result<()> {
    bail!(NOT_YET)
}

pub fn set_enabled(_name: &str, _enabled: bool) -> Result<()> {
    bail!(NOT_YET)
}

pub fn register(_name: &str, _xml: &str) -> Result<()> {
    bail!(NOT_YET)
}

pub fn delete(_name: &str) -> Result<()> {
    bail!(NOT_YET)
}
