//! Windows Task Scheduler through its COM API: task state, run now,
//! enable/disable and registration (the XML comes from `factory`).

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use windows::Win32::Foundation::{SYSTEMTIME, VARIANT_BOOL};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize};
use windows::Win32::System::TaskScheduler::{
    ITaskFolder, ITaskService, TASK_CREATE_OR_UPDATE, TASK_ENUM_HIDDEN, TASK_LOGON_INTERACTIVE_TOKEN, TASK_STATE_DISABLED, TASK_STATE_QUEUED, TASK_STATE_READY,
    TASK_STATE_RUNNING, TaskScheduler,
};
use windows::Win32::System::Time::TzSpecificLocalTimeToSystemTime;
use windows::Win32::System::Variant::{VARIANT, VariantTimeToSystemTime};
use windows::core::BSTR;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TaskInfo {
    pub name: String,
    /// `ready`, `running`, `queued`, `disabled`, `unknown`.
    pub state: String,
    pub enabled: bool,
    /// Unix ms; 0 when never / none.
    pub last_run_ms: u64,
    pub next_run_ms: u64,
    pub last_result: i32,
    pub xml: String,
}

struct Com;

impl Com {
    fn init() -> Com {
        // SAFETY: balanced by CoUninitialize in Drop; S_FALSE (already
        // initialized) also needs the matching call.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Com
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: paired with init.
        unsafe { CoUninitialize() };
    }
}

fn root() -> Result<(Com, ITaskFolder)> {
    let com = Com::init();
    // SAFETY: COM calls on an initialized apartment with valid arguments.
    unsafe {
        let service: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER).context("Task Scheduler is not available")?;
        let empty = VARIANT::default();
        service.Connect(&empty, &empty, &empty, &empty).context("cannot connect to Task Scheduler")?;
        let folder = service.GetFolder(&BSTR::from("\\"))?;
        Ok((com, folder))
    }
}

/// OLE automation date → Unix ms. Task Scheduler reports `NextRunTime` in
/// local time but `LastRunTime` in UTC (checked against launcher logs).
fn date_ms(date: f64, local_time: bool) -> u64 {
    if date <= 1.0 {
        return 0;
    }
    // SAFETY: out-parameter conversions.
    unsafe {
        let mut time = SYSTEMTIME::default();
        if VariantTimeToSystemTime(date, &mut time) == 0 {
            return 0;
        }
        if !local_time {
            return system_time_ms(&time);
        }
        let mut utc = SYSTEMTIME::default();
        if TzSpecificLocalTimeToSystemTime(None, &time, &mut utc).is_err() {
            return 0;
        }
        system_time_ms(&utc)
    }
}

fn system_time_ms(t: &SYSTEMTIME) -> u64 {
    // Days from civil (proleptic Gregorian), Howard Hinnant's algorithm.
    let (y, m, d) = (t.wYear as i64, t.wMonth as i64, t.wDay as i64);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + t.wHour as i64 * 3600 + t.wMinute as i64 * 60 + t.wSecond as i64;
    (secs.max(0) as u64) * 1000 + t.wMilliseconds as u64
}

/// Tasks in the root folder, keyed by lower-case name.
pub fn list() -> Result<BTreeMap<String, TaskInfo>> {
    let (_com, folder) = root()?;
    let mut out = BTreeMap::new();
    // SAFETY: COM calls with valid arguments; indexes are 1-based.
    unsafe {
        let tasks = folder.GetTasks(TASK_ENUM_HIDDEN.0)?;
        let count = tasks.Count()?;
        for i in 1..=count {
            let Ok(task) = tasks.get_Item(&VARIANT::from(i)) else { continue };
            let name = task.Name().map(|n| n.to_string()).unwrap_or_default();
            let state = match task.State() {
                Ok(s) if s == TASK_STATE_RUNNING => "running",
                Ok(s) if s == TASK_STATE_QUEUED => "queued",
                Ok(s) if s == TASK_STATE_READY => "ready",
                Ok(s) if s == TASK_STATE_DISABLED => "disabled",
                _ => "unknown",
            };
            let info = TaskInfo {
                state: state.into(),
                enabled: task.Enabled().is_ok_and(|e| e.as_bool()),
                last_run_ms: task.LastRunTime().map(|d| date_ms(d, false)).unwrap_or(0),
                next_run_ms: task.NextRunTime().map(|d| date_ms(d, true)).unwrap_or(0),
                last_result: task.LastTaskResult().unwrap_or(0),
                xml: task.Xml().map(|x| x.to_string()).unwrap_or_default(),
                name: name.clone(),
            };
            out.insert(name.to_lowercase(), info);
        }
    }
    Ok(out)
}

pub fn xml(name: &str) -> Result<Option<String>> {
    let (_com, folder) = root()?;
    // SAFETY: COM calls with valid arguments.
    unsafe {
        match folder.GetTask(&BSTR::from(name)) {
            Ok(task) => Ok(Some(task.Xml()?.to_string())),
            Err(_) => Ok(None),
        }
    }
}

/// Starts the task now (Task Scheduler's own overlap policy applies).
pub fn run(name: &str) -> Result<()> {
    let (_com, folder) = root()?;
    // SAFETY: COM calls with valid arguments.
    unsafe {
        let task = folder.GetTask(&BSTR::from(name)).with_context(|| format!("no task named {name}"))?;
        task.Run(&VARIANT::default()).with_context(|| format!("cannot start {name}"))?;
    }
    Ok(())
}

pub fn set_enabled(name: &str, enabled: bool) -> Result<()> {
    let (_com, folder) = root()?;
    // SAFETY: COM calls with valid arguments.
    unsafe {
        let task = folder.GetTask(&BSTR::from(name)).with_context(|| format!("no task named {name}"))?;
        task.SetEnabled(VARIANT_BOOL::from(enabled)).with_context(|| format!("cannot change {name}"))?;
    }
    Ok(())
}

/// Creates or updates a task for the current user (interactive token: runs
/// only while you are signed in, no password stored).
pub fn register(name: &str, xml: &str) -> Result<()> {
    let (_com, folder) = root()?;
    let empty = VARIANT::default();
    // SAFETY: COM calls with valid arguments.
    unsafe {
        folder
            .RegisterTask(&BSTR::from(name), &BSTR::from(xml), TASK_CREATE_OR_UPDATE.0, &empty, &empty, TASK_LOGON_INTERACTIVE_TOKEN, &empty)
            .with_context(|| format!("Task Scheduler rejected {name}"))?;
    }
    Ok(())
}

/// Removes a task (used by tests that register temporary tasks).
#[allow(dead_code)]
pub fn delete(name: &str) -> Result<()> {
    let (_com, folder) = root()?;
    // SAFETY: COM calls with valid arguments.
    unsafe { folder.DeleteTask(&BSTR::from(name), 0).with_context(|| format!("cannot delete {name}"))? };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_time_to_unix_ms() {
        let t = SYSTEMTIME { wYear: 2026, wMonth: 10, wDay: 27, wHour: 20, wMinute: 0, wSecond: 0, wMilliseconds: 5, ..Default::default() };
        assert_eq!(system_time_ms(&t), 1_793_131_200_005);
        let epoch = SYSTEMTIME { wYear: 1970, wMonth: 1, wDay: 1, ..Default::default() };
        assert_eq!(system_time_ms(&epoch), 0);
    }

    #[test]
    #[ignore = "reads the real Task Scheduler"]
    fn lists_real_tasks() {
        let tasks = list().unwrap();
        for t in tasks.values() {
            println!("{} {} enabled={} last={} next={} result={:#x}", t.name, t.state, t.enabled, t.last_run_ms, t.next_run_ms, t.last_result);
        }
    }
}
