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
