//! Windows platform helpers: process liveness, process trees, job objects, resource usage.

use std::io;
use std::path::Path;

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, GetProcessTimes, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE, TerminateProcess,
};

const STILL_ACTIVE: u32 = 259;

struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

fn open(pid: u32, access: u32) -> Option<Owned> {
    let handle = unsafe { OpenProcess(access, 0, pid) };
    (!handle.is_null()).then_some(Owned(handle))
}

fn filetime_u64(time: &FILETIME) -> u64 {
    ((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64
}

/// Unix milliseconds when `pid` was created, if it is still running.
pub fn process_start_ms(pid: u32) -> Option<u64> {
    if pid == 0 {
        return None;
    }
    let process = open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut code = 0u32;
    if unsafe { GetExitCodeProcess(process.0, &mut code) } == 0 || code != STILL_ACTIVE {
        return None;
    }
    let mut created = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let mut exited = created;
    let mut kernel = created;
    let mut user = created;
    if unsafe { GetProcessTimes(process.0, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return None;
    }
    Some((filetime_u64(&created) / 10_000).saturating_sub(11_644_473_600_000))
}

/// True when `pid` is running and was already running when `lock` was last written.
/// Guards against a recycled PID making a stale lock look live.
pub fn lock_owner_alive(pid: u32, lock: &Path) -> bool {
    let Some(started) = process_start_ms(pid) else { return false };
    let written = std::fs::metadata(lock)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64);
    match written {
        Some(written) => started <= written + 2_000,
        None => true,
    }
}

/// Terminates `root` and every descendant process (deepest first).
pub fn kill_tree(root: u32) -> io::Result<usize> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let snapshot = Owned(snapshot);
    let mut pairs = Vec::new();
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    if unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0 {
        loop {
            pairs.push((entry.th32ProcessID, entry.th32ParentProcessID));
            if unsafe { Process32NextW(snapshot.0, &mut entry) } == 0 {
                break;
            }
        }
    }
    let mut order = vec![root];
    let mut index = 0;
    while index < order.len() {
        let parent = order[index];
        for &(pid, ppid) in &pairs {
            // A recycled parent PID could otherwise adopt unrelated processes.
            if ppid == parent && pid != parent && !order.contains(&pid) && started_after(pid, parent) {
                order.push(pid);
            }
        }
        index += 1;
    }
    let mut killed = 0;
    for pid in order.into_iter().rev() {
        if let Some(process) = open(pid, PROCESS_TERMINATE)
            && unsafe { TerminateProcess(process.0, 1) } != 0
        {
            killed += 1;
        }
    }
    Ok(killed)
}

fn started_after(child: u32, parent: u32) -> bool {
    match (process_start_ms(child), process_start_ms(parent)) {
        (Some(c), Some(p)) => c >= p,
        (Some(_), None) => true,
        _ => false,
    }
}

/// Places the current process in a job that kills all descendants when the process exits.
pub fn contain_descendants() -> io::Result<()> {
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
            || AssignProcessToJobObject(job, GetCurrentProcess()) == 0
        {
            let error = io::Error::last_os_error();
            CloseHandle(job);
            return Err(error);
        }
        // Intentionally leaked: closing the last handle kills the job.
        Ok(())
    }
}

/// A job that kills the processes put in it when it is dropped (or when this
/// process exits, which closes the handle): managed helper processes never
/// outlive their owner.
pub struct KillOnClose(HANDLE);

// SAFETY: a job handle may be used and closed from any thread.
unsafe impl Send for KillOnClose {}
unsafe impl Sync for KillOnClose {}

impl KillOnClose {
    /// Puts `child` (and the processes it starts) in a new kill-on-close job.
    pub fn new(child: &std::process::Child) -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: a fresh job with a valid limit structure; `child` holds a live process handle.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(io::Error::last_os_error());
            }
            let guard = KillOnClose(job);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(job, JobObjectExtendedLimitInformation, &info as *const _ as *const _, std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32) == 0
                || AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(guard)
        }
    }
}

impl Drop for KillOnClose {
    fn drop(&mut self) {
        // SAFETY: the job handle this guard owns; closing it kills the job's processes.
        unsafe { CloseHandle(self.0) };
    }
}

/// True while `pid` runs.
pub fn alive(pid: u32) -> bool {
    process_start_ms(pid).is_some()
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Usage {
    pub working_set: u64,
    pub private: u64,
    pub cpu_ms: u64,
}

pub fn process_usage(pid: Option<u32>) -> Usage {
    let process = match pid {
        Some(pid) => match open(pid, PROCESS_QUERY_LIMITED_INFORMATION) {
            Some(handle) => handle,
            None => return Usage::default(),
        },
        None => Owned(std::ptr::null_mut()),
    };
    let handle = if pid.is_some() { process.0 } else { unsafe { GetCurrentProcess() } };
    let mut usage = Usage::default();
    unsafe {
        let mut counters: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
        counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        if GetProcessMemoryInfo(handle, &mut counters as *mut _ as *mut PROCESS_MEMORY_COUNTERS, counters.cb) != 0 {
            usage.working_set = counters.WorkingSetSize as u64;
            usage.private = counters.PrivateUsage as u64;
        }
        let mut created = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let mut exited = created;
        let mut kernel = created;
        let mut user = created;
        if GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) != 0 {
            usage.cpu_ms = (filetime_u64(&kernel) + filetime_u64(&user)) / 10_000;
        }
    }
    usage
}

/// Descendant process ids of `root` (excluding root).
pub fn descendants(root: u32) -> Vec<u32> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let snapshot = Owned(snapshot);
    let mut pairs = Vec::new();
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    if unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0 {
        loop {
            pairs.push((entry.th32ProcessID, entry.th32ParentProcessID));
            if unsafe { Process32NextW(snapshot.0, &mut entry) } == 0 {
                break;
            }
        }
    }
    let mut found = vec![root];
    let mut index = 0;
    while index < found.len() {
        let parent = found[index];
        for &(pid, ppid) in &pairs {
            if ppid == parent && pid != parent && !found.contains(&pid) {
                found.push(pid);
            }
        }
        index += 1;
    }
    found.remove(0);
    found
}

/// String SID of the current user, e.g. `S-1-5-21-...`.
pub fn current_user_sid() -> io::Result<String> {
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        token_user_sid(&Owned(token))
    }
}

/// String SID of the user process `pid` runs as.
pub fn process_user_sid(pid: u32) -> io::Result<String> {
    let process = open(pid, PROCESS_QUERY_LIMITED_INFORMATION).ok_or_else(io::Error::last_os_error)?;
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: a valid process handle; the token handle is owned below.
    if unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    token_user_sid(&Owned(token))
}

/// The user SID of an access token as a string.
fn token_user_sid(token: &Owned) -> io::Result<String> {
    unsafe {
        let mut needed = 0u32;
        GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
        if GetTokenInformation(token.0, TokenUser, buffer.as_mut_ptr() as *mut _, needed, &mut needed) == 0 {
            return Err(io::Error::last_os_error());
        }
        let user = &*(buffer.as_ptr() as *const TOKEN_USER);
        let mut text: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut len = 0;
        while *text.add(len) != 0 {
            len += 1;
        }
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text as _);
        Ok(sid)
    }
}

/// Creates a new file only the current user can open (a protected DACL);
/// fails when it exists.
pub fn create_private(path: &Path) -> io::Result<std::fs::File> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::GENERIC_WRITE;
    use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::Storage::FileSystem::{CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL};

    let sid = current_user_sid()?;
    let sddl: Vec<u16> = format!("D:P(A;;FA;;;{sid})").encode_utf16().chain(Some(0)).collect();
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated; the descriptor is freed below.
    if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut descriptor, std::ptr::null_mut()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let attributes = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: descriptor, bInheritHandle: 0 };
    // SAFETY: `wide` is NUL-terminated and `attributes` outlives the call.
    let handle = unsafe { CreateFileW(wide.as_ptr(), GENERIC_WRITE, 0, &attributes, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, std::ptr::null_mut()) };
    let error = io::Error::last_os_error();
    // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
    unsafe { LocalFree(descriptor) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(error);
    }
    // SAFETY: a new, owned file handle.
    Ok(unsafe { std::fs::File::from_raw_handle(handle) })
}

/// Writes `path` atomically with an owner-only DACL: a private temporary file
/// (`NAME.<random>.tmp` next to it) renamed over it.
pub fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let name = path.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?.to_string_lossy().into_owned();
    let temp = path.with_file_name(format!("{name}.{}.tmp", crate::paths::hex(&crate::protocol::nonce()[..8])));
    let written = create_private(&temp).and_then(|mut file| {
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)
    });
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written
}

/// Runs this process above normal priority so busy sessions (Copilot starting,
/// tools running) cannot starve the UI. Child processes still start at normal
/// priority: Windows only passes idle/below-normal classes on to children.
pub fn raise_priority() {
    use windows_sys::Win32::System::Threading::{ABOVE_NORMAL_PRIORITY_CLASS, GetCurrentProcess, SetPriorityClass};
    // SAFETY: pseudo handle of the current process.
    unsafe {
        SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS);
    }
}

/// (pid, parent pid) of every process.
pub fn process_pairs() -> Vec<(u32, u32)> {
    process_entries().into_iter().map(|(pid, parent, _)| (pid, parent)).collect()
}

/// (pid, parent pid, lowercase executable name) of every process.
pub fn process_entries() -> Vec<(u32, u32, String)> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let snapshot = Owned(snapshot);
    let mut list = Vec::new();
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    if unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0 {
        loop {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase();
            list.push((entry.th32ProcessID, entry.th32ParentProcessID, name));
            if unsafe { Process32NextW(snapshot.0, &mut entry) } == 0 {
                break;
            }
        }
    }
    list
}

/// `root` and all its descendants, in O(processes). Children must have started
/// after their parent, so a recycled parent PID cannot adopt unrelated processes.
pub fn process_tree(root: u32, children: &std::collections::HashMap<u32, Vec<u32>>) -> Vec<u32> {
    let mut found = vec![root];
    let mut index = 0;
    while index < found.len() {
        let parent = found[index];
        for &child in children.get(&parent).map(Vec::as_slice).unwrap_or_default() {
            if child != parent && !found.contains(&child) && started_after(child, parent) {
                found.push(child);
            }
        }
        index += 1;
    }
    found
}

/// Sets a process's priority class (`ABOVE_NORMAL_PRIORITY_CLASS`, …).
pub fn set_priority_class(pid: u32, class: u32) -> bool {
    use windows_sys::Win32::System::Threading::{PROCESS_SET_INFORMATION, SetPriorityClass};
    match open(pid, PROCESS_SET_INFORMATION) {
        // SAFETY: valid process handle with PROCESS_SET_INFORMATION access.
        Some(process) => unsafe { SetPriorityClass(process.0, class) != 0 },
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_is_alive_and_measurable() {
        let started = process_start_ms(std::process::id()).expect("own process is alive");
        assert!(started <= crate::paths::now_ms());
        assert!(process_start_ms(0).is_none());
        let usage = process_usage(None);
        assert!(usage.working_set > 0);
        assert!(current_user_sid().unwrap().starts_with("S-1-"));
    }

    #[test]
    fn a_kill_on_close_job_ends_its_process_when_dropped() {
        let mut child = std::process::Command::new("cmd.exe").args(["/d", "/c", "ping -n 30 127.0.0.1 >nul"]).spawn().unwrap();
        let job = KillOnClose::new(&child).unwrap();
        assert!(alive(child.id()));
        drop(job);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() {
            assert!(std::time::Instant::now() < deadline, "the process outlived its job");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn recycled_pid_cannot_make_an_old_lock_live() {
        let dir = std::env::temp_dir().join(format!("cc-lock-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("inuse.lock");
        std::fs::write(&lock, "x").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(10 * 365 * 86_400);
        std::fs::File::options().write(true).open(&lock).unwrap().set_modified(old).unwrap();
        assert!(!lock_owner_alive(std::process::id(), &lock));
        std::fs::File::options().write(true).open(&lock).unwrap().set_modified(std::time::SystemTime::now()).unwrap();
        assert!(lock_owner_alive(std::process::id(), &lock));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
