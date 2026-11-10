//! Windows: today's `canvas_common::win` helpers, unchanged.

pub use canvas_common::win::{
    KillOnClose, Usage, alive, contain_descendants, descendants, kill_tree, lock_owner_alive, process_entries, process_pairs, process_start_ms, process_tree,
    process_usage, raise_priority,
};
pub use canvas_common::win::{current_user_sid as current_user_id, process_user_sid as process_user_id};

use super::Class;

/// Sets the priority class of `pid` for its session's class: above normal
/// for the focused session, normal for visible ones, below normal otherwise.
pub fn set_priority(pid: u32, class: Class) -> bool {
    use windows_sys::Win32::System::Threading::{ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS};
    let class = match class {
        Class::Focused => ABOVE_NORMAL_PRIORITY_CLASS,
        Class::Visible => NORMAL_PRIORITY_CLASS,
        Class::Background => BELOW_NORMAL_PRIORITY_CLASS,
    };
    canvas_common::win::set_priority_class(pid, class)
}

/// The command line of `pid` is not read on Windows (process names suffice there).
pub fn process_argv(_pid: u32) -> Option<Vec<String>> {
    None
}

/// Open handles of `pid` (the soak test watches for leaks).
pub fn handle_count(pid: u32) -> u32 {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{GetProcessHandleCount, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: plain Win32 calls with a handle closed right after.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return 0;
        }
        let mut count = 0;
        let ok = GetProcessHandleCount(handle, &mut count);
        CloseHandle(handle);
        if ok != 0 { count } else { 0 }
    }
}

/// Windows has no per-process descriptor limit to raise.
pub fn raise_fd_limit() -> u64 {
    0
}

// Process suspension (whole process, every thread) from ntdll; linked by
// name, so no import library is needed.
#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    fn NtSuspendProcess(process: windows_sys::Win32::Foundation::HANDLE) -> i32;
    fn NtResumeProcess(process: windows_sys::Win32::Foundation::HANDLE) -> i32;
}

/// Suspends (or resumes) `pid` only while it still has the start time
/// `started`: the start time is read through the handle used for the call,
/// and an open handle keeps the pid from being reused. `Ok(false)`: gone, or
/// not this process any more.
fn suspend_started(pid: u32, started: u64, suspend: bool) -> std::io::Result<bool> {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, FILETIME};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SUSPEND_RESUME};
    const STILL_ACTIVE: u32 = 259;
    if pid == 0 || pid == std::process::id() {
        return Ok(false);
    }
    // SAFETY: plain Win32 calls; the handle is closed below.
    let handle = unsafe { OpenProcess(PROCESS_SUSPEND_RESUME | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) { Ok(false) } else { Err(error) };
    }
    let mut code = 0u32;
    let mut created = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut exited, mut kernel, mut user) = (created, created, created);
    // SAFETY: a valid process handle and out-pointers.
    let same = unsafe { GetExitCodeProcess(handle, &mut code) != 0 && code == STILL_ACTIVE && GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) != 0 } && {
        let ticks = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
        (ticks / 10_000).saturating_sub(11_644_473_600_000) == started
    };
    // SAFETY: the handle has PROCESS_SUSPEND_RESUME access.
    let status = if same { unsafe { if suspend { NtSuspendProcess(handle) } else { NtResumeProcess(handle) } } } else { 0 };
    // SAFETY: the handle opened above.
    unsafe { CloseHandle(handle) };
    if status < 0 {
        return Err(std::io::Error::other(format!("{} of process {pid} failed: NTSTATUS 0x{status:08x}", if suspend { "suspension" } else { "resumption" })));
    }
    Ok(same)
}

/// `root` (when it still has the start time `started`) and its descendants,
/// breadth first, each with its start time. Never this process's tree.
fn started_tree(root: u32, started: u64) -> Vec<(u32, u64)> {
    if root == 0 || root == std::process::id() || process_start_ms(root) != Some(started) {
        return Vec::new();
    }
    let mut children: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
    for (pid, parent, _) in process_entries() {
        children.entry(parent).or_default().push(pid);
    }
    process_tree(root, &children).into_iter().filter_map(|pid| if pid == root { Some((pid, started)) } else { process_start_ms(pid).map(|start| (pid, start)) }).collect()
}

/// Freezes `root` and every descendant (`NtSuspendProcess`: every thread of
/// each), deepest first and the root last, as a memory suspend stops
/// everything in a VM (`vm::local`). `started_ms` is `root`'s recorded start
/// time: a root without it (gone, or its pid reused) is left alone. Every
/// process is checked against its start time just before it is suspended,
/// and children that appeared meanwhile are frozen by a further pass. Each
/// process is suspended once (suspensions count: [`resume_tree`] undoes one).
/// Returns how many processes were frozen.
pub fn suspend_tree(root: u32, started_ms: u64) -> std::io::Result<usize> {
    let mut seen = std::collections::HashSet::new();
    let mut frozen = 0;
    for _ in 0..4 {
        let pending: Vec<(u32, u64)> = started_tree(root, started_ms).into_iter().filter(|(pid, _)| !seen.contains(pid)).collect();
        if pending.is_empty() {
            break;
        }
        for (pid, start) in pending.into_iter().rev() {
            seen.insert(pid);
            match suspend_started(pid, start, true) {
                Err(error) if pid == root => return Err(error),
                Ok(true) => frozen += 1,
                _ => {}
            }
        }
    }
    Ok(frozen)
}

/// Thaws what [`suspend_tree`] froze: `root` first (when it still has the
/// start time `started_ms`), then its descendants, each checked against its
/// start time. Returns how many processes were resumed.
pub fn resume_tree(root: u32, started_ms: u64) -> std::io::Result<usize> {
    let mut resumed = 0;
    for (pid, start) in started_tree(root, started_ms) {
        match suspend_started(pid, start, false) {
            Err(error) if pid == root => return Err(error),
            Ok(true) => resumed += 1,
            _ => {}
        }
    }
    Ok(resumed)
}
