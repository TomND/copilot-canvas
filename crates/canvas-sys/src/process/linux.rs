//! Linux: the process table from `/proc`.

use std::io;
use std::os::unix::fs::MetadataExt;
use std::sync::OnceLock;

use super::Class;

/// The fields of `/proc/<pid>/stat` after the command name, and the name.
fn stat(pid: u32) -> Option<(String, Vec<String>)> {
    if pid == 0 {
        return None;
    }
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let name = text.get(open + 1..close)?.to_owned();
    Some((name, text[close + 1..].split_whitespace().map(str::to_owned).collect()))
}

fn ticks_per_second() -> u64 {
    static TICKS: OnceLock<u64> = OnceLock::new();
    // SAFETY: sysconf has no preconditions.
    *TICKS.get_or_init(|| match unsafe { libc::sysconf(libc::_SC_CLK_TCK) } {
        ticks if ticks > 0 => ticks as u64,
        _ => 100,
    })
}

/// Boot time in Unix milliseconds (`btime` of `/proc/stat`).
fn boot_ms() -> u64 {
    static BOOT: OnceLock<u64> = OnceLock::new();
    *BOOT.get_or_init(|| {
        std::fs::read_to_string("/proc/stat")
            .ok()
            .and_then(|text| text.lines().find_map(|line| line.strip_prefix("btime ").and_then(|v| v.trim().parse::<u64>().ok())))
            .unwrap_or(0)
            * 1000
    })
}

/// Unix milliseconds when `pid` was created, if it is still running (not a zombie).
pub fn process_start_ms(pid: u32) -> Option<u64> {
    let (_, fields) = stat(pid)?;
    // Fields after the name: state is field 3 of the line, starttime field 22.
    if matches!(fields.first().map(String::as_str), Some("Z" | "X" | "x")) {
        return None;
    }
    let ticks: u64 = fields.get(19)?.parse().ok()?;
    Some(boot_ms() + ticks * 1000 / ticks_per_second())
}

/// (pid, parent pid, lowercase executable name) of every process.
pub fn process_entries() -> Vec<(u32, u32, String)> {
    let Ok(dir) = std::fs::read_dir("/proc") else { return Vec::new() };
    dir.filter_map(|entry| {
        let pid: u32 = entry.ok()?.file_name().to_str()?.parse().ok()?;
        let (name, fields) = stat(pid)?;
        let parent: u32 = fields.get(1)?.parse().ok()?;
        Some((pid, parent, name.to_lowercase()))
    })
    .collect()
}

/// The command line of `pid` (`/proc/<pid>/cmdline`).
pub fn process_argv(pid: u32) -> Option<Vec<String>> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(&bytes);
    Some(bytes.split(|&b| b == 0).map(|arg| String::from_utf8_lossy(arg).into_owned()).collect())
}

pub fn usage(pid: u32) -> super::Usage {
    // SAFETY: sysconf has no preconditions.
    let page = match unsafe { libc::sysconf(libc::_SC_PAGESIZE) } {
        size if size > 0 => size as u64,
        _ => 4096,
    };
    let mut usage = super::Usage::default();
    if let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/statm")) {
        let pages: Vec<u64> = text.split_whitespace().filter_map(|v| v.parse().ok()).collect();
        if let (Some(resident), Some(shared)) = (pages.get(1), pages.get(2)) {
            usage.working_set = resident * page;
            usage.private = resident.saturating_sub(*shared) * page;
        }
    }
    if let Some((_, fields)) = stat(pid) {
        let ticks: u64 = [11, 12].iter().filter_map(|&i| fields.get(i)?.parse::<u64>().ok()).sum();
        usage.cpu_ms = ticks * 1000 / ticks_per_second();
    }
    usage
}

pub fn process_uid(pid: u32) -> io::Result<u32> {
    std::fs::metadata(format!("/proc/{pid}")).map(|meta| meta.uid())
}

/// Orphaned descendants re-parent to this process instead of init, so the
/// service still sees (and stops) them.
pub fn contain_descendants() -> io::Result<()> {
    // SAFETY: prctl with integer arguments.
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn raise_self() {}

/// Background sessions are reniced to 5; an unprivileged process cannot
/// lower a nice value again, so visible and focused sessions are left as
/// they are (the cgroup weights of MAC-LINUX-PLAN §5.3 come with delegation).
pub fn set_priority(pid: u32, class: Class) -> bool {
    if class != Class::Background {
        return true;
    }
    // SAFETY: getpriority/setpriority on a checked pid.
    unsafe {
        *libc::__errno_location() = 0;
        let nice = libc::getpriority(libc::PRIO_PROCESS, pid as libc::id_t);
        nice >= 5 || libc::setpriority(libc::PRIO_PROCESS, pid as libc::id_t, 5) == 0
    }
}

pub fn fd_count(pid: u32) -> u32 {
    std::fs::read_dir(format!("/proc/{pid}/fd")).map(|d| d.count() as u32).unwrap_or(0)
}
