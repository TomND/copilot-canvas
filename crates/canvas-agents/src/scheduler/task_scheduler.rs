//! Windows Task Scheduler behind the [`Scheduler`] trait: the COM calls of
//! `taskschd` and the XML of `factory`, exactly as before the trait.

use std::collections::BTreeMap;

use anyhow::Result;

use super::{Scheduler, TaskInfo};
use crate::factory::{self, Defaults, Imported, Manifest, Principal};
use crate::taskschd;

pub struct TaskScheduler;

impl Scheduler for TaskScheduler {
    fn name(&self) -> &'static str {
        "Task Scheduler"
    }

    fn list(&self) -> Result<BTreeMap<String, TaskInfo>> {
        taskschd::list()
    }

    fn definition(&self, name: &str) -> Result<Option<String>> {
        taskschd::xml(name)
    }

    /// The task's XML only (all its drift needs), without listing the folder.
    fn task(&self, name: &str) -> Result<Option<TaskInfo>> {
        Ok(taskschd::xml(name)?.map(|xml| TaskInfo { name: name.to_owned(), xml, ..TaskInfo::default() }))
    }

    fn run(&self, name: &str) -> Result<()> {
        taskschd::run(name)
    }

    fn set_enabled(&self, name: &str, enabled: bool) -> Result<()> {
        taskschd::set_enabled(name, enabled)
    }

    fn register(&self, name: &str, definition: &str) -> Result<()> {
        taskschd::register(name, definition)
    }

    fn delete(&self, name: &str) -> Result<()> {
        taskschd::delete(name)
    }

    /// The task XML at the current UTC offset, run as the installed task's
    /// principal (else the current user).
    fn render(&self, m: &Manifest, d: &Defaults, existing: Option<&TaskInfo>) -> String {
        let offset = factory::local_offset_minutes();
        let imported = existing.map(|t| factory::import_xml(&m.name, &t.xml, offset));
        let current = Principal::current();
        let who = Principal {
            user_id: imported.as_ref().map(|i| i.user_id.clone()).filter(|u| !u.is_empty()).unwrap_or(current.user_id),
            logon_user: imported.as_ref().map(|i| i.logon_user.clone()).filter(|u| !u.is_empty()).unwrap_or(current.logon_user),
        };
        factory::task_xml(m, d, &who, offset)
    }

    fn import(&self, task: &TaskInfo) -> Imported {
        factory::import_xml(&task.name, &task.xml, factory::local_offset_minutes())
    }

    fn drift(&self, m: &Manifest, d: &Defaults, task: &TaskInfo) -> Vec<String> {
        factory::drift(m, d, &factory::import_xml(&m.name, &task.xml, factory::local_offset_minutes()))
    }

    /// Tasks run by a Python launcher with a task file.
    fn importable(&self, task: &TaskInfo) -> bool {
        (task.xml.contains(".py&quot;") || task.xml.contains(".py\"")) && task.xml.contains(".json")
    }

    fn launched_by(&self, task: &TaskInfo, d: &Defaults) -> bool {
        !d.launcher.is_empty() && task.xml.to_lowercase().contains(&d.launcher.to_lowercase())
    }
}
