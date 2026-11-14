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

/// The class `pid` runs in now (`None`: unreadable, or a class the priority
/// manager never sets).
pub fn priority(pid: u32) -> Option<Class> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, GetPriorityClass, NORMAL_PRIORITY_CLASS, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: plain Win32 calls with a handle closed right after.
    let class = unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let class = GetPriorityClass(handle);
        CloseHandle(handle);
        class
    };
    match class {
        ABOVE_NORMAL_PRIORITY_CLASS => Some(Class::Focused),
        NORMAL_PRIORITY_CLASS => Some(Class::Visible),
        BELOW_NORMAL_PRIORITY_CLASS => Some(Class::Background),
        _ => None,
    }
}

/// Whether `name` (lowercase, as [`process_entries`] gives it) is a
/// pseudoconsole host: the system's `conhost.exe`, or the `OpenConsole.exe`
/// that comes with a side-loaded `conpty.dll`.
pub fn is_console_host(name: &str) -> bool {
    matches!(name, "conhost.exe" | "openconsole.exe")
}

/// The pseudoconsole hosts this process started, with their start times.
/// `CreatePseudoConsole` starts one per terminal as a child of the calling
/// process (not of the program in the terminal); it carries every keystroke
/// in and every byte of output out.
pub fn console_hosts() -> Vec<(u32, u64)> {
    let me = std::process::id();
    let Some(my_start) = process_start_ms(me) else { return Vec::new() };
    process_entries()
        .into_iter()
        .filter(|(_, parent, name)| *parent == me && is_console_host(name))
        // A process whose parent had this pid before us is not ours.
        .filter_map(|(pid, _, _)| process_start_ms(pid).filter(|start| *start >= my_start).map(|start| (pid, start)))
        .collect()
}

/// A job that follows one process and everything it starts (a terminal's
/// agent), so what the agent leaves can be found after the parents between
/// them exited, which a tree walk cannot do. Closing it ends nothing
/// (R2-B-01: as a kill-on-close job it ended every server, VS Code and
/// shared build server the agent ever started); the terminal ends only what
/// [`super::leftovers`] picks from [`TreeJob::pids`]. Processes may break
/// away (`CREATE_BREAKAWAY_FROM_JOB`) as they may from the service's own
/// job. Nested in that job (Windows 8 and later).
pub struct TreeJob(windows_sys::Win32::Foundation::HANDLE);

// SAFETY: a job handle may be used and closed from any thread.
unsafe impl Send for TreeJob {}
unsafe impl Sync for TreeJob {}

impl TreeJob {
    /// Puts the process behind `process` (a handle with `PROCESS_SET_QUOTA`
    /// and `PROCESS_TERMINATE` access) in a new job.
    pub fn new(process: std::os::windows::io::RawHandle) -> std::io::Result<Self> {
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        // SAFETY: a fresh job with a valid limit structure; the guard closes it on failure.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let guard = TreeJob(job);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_BREAKAWAY_OK;
            if SetInformationJobObject(job, JobObjectExtendedLimitInformation, &info as *const _ as *const _, std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32) == 0
                || AssignProcessToJobObject(job, process as _) == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(guard)
        }
    }

    /// The processes in the job now (those that broke away are not).
    pub fn pids(&self) -> Vec<u32> {
        use windows_sys::Win32::System::JobObjects::{JobObjectBasicProcessIdList, QueryInformationJobObject};
        // Two counts (8 bytes), then the ids.
        let header = 8 / std::mem::size_of::<usize>();
        let mut room = 64usize;
        loop {
            let mut buffer = vec![0usize; header + room];
            // SAFETY: `buffer` has the byte size given, aligned for the structure.
            let ok = unsafe {
                QueryInformationJobObject(self.0, JobObjectBasicProcessIdList, buffer.as_mut_ptr().cast(), (buffer.len() * std::mem::size_of::<usize>()) as u32, std::ptr::null_mut())
            } != 0;
            // SAFETY: the two counts lead the buffer.
            let (assigned, listed) = unsafe {
                let counts = buffer.as_ptr() as *const u32;
                (*counts as usize, *counts.add(1) as usize)
            };
            if ok && listed <= room {
                return buffer[header..header + listed].iter().map(|&pid| pid as u32).collect();
            }
            if room >= 1 << 16 {
                return Vec::new();
            }
            room = (assigned + 16).max(room * 2).min(1 << 16);
        }
    }

    /// Puts `pid` in this job when it is in another job but not in this one
    /// (an agent's child started before the agent was put in its job, which
    /// inherited the service's job instead, R2-B-03). A process in no job
    /// broke away on purpose and is left so. Answers whether it was put in.
    pub fn adopt(&self, pid: u32) -> bool {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, IsProcessInJob};
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE};
        if pid == 0 || pid == std::process::id() {
            return false;
        }
        // SAFETY: plain Win32 calls with a handle closed right after.
        unsafe {
            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return false;
            }
            let (mut here, mut any) = (0, 0);
            let adopted = IsProcessInJob(process, self.0, &mut here) != 0
                && here == 0
                && IsProcessInJob(process, std::ptr::null_mut(), &mut any) != 0
                && any != 0
                && AssignProcessToJobObject(self.0, process) != 0;
            CloseHandle(process);
            adopted
        }
    }

    /// [`TreeJob::adopt`] for each descendant of `root` (still the process
    /// started at `started`) missing from the job; answers how many.
    pub fn adopt_tree(&self, root: u32, started: u64) -> usize {
        started_tree(root, started).into_iter().filter(|(pid, _)| *pid != root).filter(|(pid, _)| self.adopt(*pid)).count()
    }
}

impl Drop for TreeJob {
    fn drop(&mut self) {
        // SAFETY: the job handle this guard owns; its processes keep running.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// The lowercase file name of `pid`'s executable (`copilot.exe`).
pub fn image_name(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
    // SAFETY: plain Win32 calls with a handle closed right after; the buffer's size is passed.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        let mut buffer = [0u16; 1024];
        let mut size = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut size) != 0;
        CloseHandle(process);
        let path = String::from_utf16_lossy(&buffer[..size as usize]);
        ok.then(|| path.rsplit(['\\', '/']).next().unwrap_or_default().to_lowercase()).filter(|name| !name.is_empty())
    }
}

/// The console host `pid` is attached to (`conhost.exe` or `OpenConsole.exe`;
/// a terminal's is its pseudoconsole's host): `None` for a process without
/// a console (a GUI program, a detached one) or one that cannot be read.
/// A process started with a console of its own (`Start-Process`, `start`,
/// `CREATE_NO_WINDOW`) has another host than the terminal it came from.
pub fn console_of(pid: u32) -> Option<u32> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // `ProcessConsoleHostProcess`: the host's pid, its two low bits flags.
    const PROCESS_CONSOLE_HOST_PROCESS: u32 = 49;
    if pid == 0 {
        return None;
    }
    // SAFETY: plain calls with a handle closed right after; the output is one usize.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        let mut host = 0usize;
        let status = NtQueryInformationProcess(process, PROCESS_CONSOLE_HOST_PROCESS, (&mut host as *mut usize).cast(), std::mem::size_of::<usize>() as u32, std::ptr::null_mut());
        CloseHandle(process);
        (status >= 0).then_some((host & !3) as u32).filter(|host| *host != 0)
    }
}

/// Kills `pid` alone (not its descendants) while it still has the start
/// time `started`; `false` when it is gone or another process now.
pub fn kill_started(pid: u32, started: u64) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, TerminateProcess};
    const STILL_ACTIVE: u32 = 259;
    if pid == 0 || pid == std::process::id() {
        return false;
    }
    // SAFETY: plain Win32 calls; the open handle keeps the pid from being reused meanwhile.
    unsafe {
        let process = OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return false;
        }
        let mut code = 0u32;
        let mut created = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut exited, mut kernel, mut user) = (created, created, created);
        let same = GetExitCodeProcess(process, &mut code) != 0 && code == STILL_ACTIVE && GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) != 0 && {
            let ticks = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
            (ticks / 10_000).saturating_sub(11_644_473_600_000) == started
        };
        let killed = same && TerminateProcess(process, 1) != 0;
        CloseHandle(process);
        killed
    }
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

// Process suspension (whole process, every thread) and the console host
// query from ntdll; linked by name, so no import library is needed.
#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    fn NtSuspendProcess(process: windows_sys::Win32::Foundation::HANDLE) -> i32;
    fn NtResumeProcess(process: windows_sys::Win32::Foundation::HANDLE) -> i32;
    fn NtQueryInformationProcess(process: windows_sys::Win32::Foundation::HANDLE, class: u32, information: *mut core::ffi::c_void, length: u32, returned: *mut u32) -> i32;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;

    fn ping(flags: u32) -> std::process::Child {
        std::process::Command::new("ping.exe").args(["-n", "30", "127.0.0.1"]).stdout(std::process::Stdio::null()).creation_flags(flags).spawn().expect("ping.exe")
    }

    fn in_any_job(child: &std::process::Child) -> bool {
        let mut any = 0;
        // SAFETY: a live process handle and an out-pointer.
        unsafe { windows_sys::Win32::System::JobObjects::IsProcessInJob(child.as_raw_handle() as _, std::ptr::null_mut(), &mut any) != 0 && any != 0 }
    }

    /// R2-B-01 and R2-B-03's facts: an agent's job lists its processes and
    /// ends none when closed; a process missing from it is put in unless it
    /// is in no job (it broke away); consoles tell a terminal's processes
    /// from those with a console of their own or none; a kill names one process.
    #[test]
    fn a_tree_job_follows_an_agent_without_ending_anything() {
        const CREATE_NEW_CONSOLE: u32 = 0x10;
        const DETACHED_PROCESS: u32 = 0x8;
        let mut member = ping(0);
        let job = TreeJob::new(member.as_raw_handle()).expect("a job");
        // (With the console host Windows starts for it when this process has none.)
        assert!(job.pids().contains(&member.id()), "{:?}", job.pids());
        assert_eq!(image_name(member.id()).as_deref(), Some("ping.exe"));
        let mut stray = ping(0);
        let expected = in_any_job(&stray);
        assert_eq!(job.adopt(stray.id()), expected, "adopted only from another job (in a job: {expected})");
        if expected {
            assert!(job.pids().contains(&stray.id()));
            assert!(!job.adopt(stray.id()), "once");
        }
        drop(job);
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(alive(member.id()) && alive(stray.id()), "closing the job ends nothing");

        let mut own = ping(CREATE_NEW_CONSOLE);
        let mut none = ping(DETACHED_PROCESS);
        std::thread::sleep(std::time::Duration::from_millis(300));
        let host = console_of(own.id()).expect("a console of its own");
        assert_ne!(Some(host), console_of(member.id()), "another console than its parent's");
        assert_eq!(console_of(none.id()), None, "a detached process has no console");

        let started = process_start_ms(member.id()).unwrap();
        assert!(!kill_started(member.id(), started + 1), "not another process of that pid");
        assert!(kill_started(member.id(), started));
        member.wait().unwrap();
        assert!(!kill_started(member.id(), started), "gone");
        for child in [&mut stray, &mut own, &mut none] {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
