//! macOS: the process table through libproc (`proc_listallpids`,
//! `proc_pidinfo`, `proc_pid_rusage`) and `sysctl(KERN_PROCARGS2)` for
//! command lines.

use std::io;
use std::mem::{MaybeUninit, size_of};
use std::sync::OnceLock;

use super::Class;

fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    let size = size_of::<libc::proc_bsdinfo>() as i32;
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    // SAFETY: `info` has room for one proc_bsdinfo.
    let read = unsafe { libc::proc_pidinfo(pid as i32, libc::PROC_PIDTBSDINFO, 0, info.as_mut_ptr().cast(), size) };
    // SAFETY: proc_pidinfo filled the whole structure.
    (read == size).then(|| unsafe { info.assume_init() })
}

fn text(chars: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = chars.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Unix milliseconds when `pid` was created, if it is still running (not a zombie).
pub fn process_start_ms(pid: u32) -> Option<u64> {
    let info = bsd_info(pid)?;
    if info.pbi_status == libc::SZOMB {
        return None;
    }
    Some(info.pbi_start_tvsec * 1000 + info.pbi_start_tvusec / 1000)
}

pub(crate) fn all_pids() -> Vec<u32> {
    // SAFETY: a null buffer asks for the count.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Vec::new();
    }
    // Room for processes started meanwhile.
    let mut pids = vec![0i32; count as usize + 64];
    // SAFETY: `pids` has the byte size given.
    let found = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * size_of::<i32>()) as i32) };
    pids.truncate(found.max(0) as usize);
    pids.into_iter().filter(|&pid| pid > 0).map(|pid| pid as u32).collect()
}

/// (pid, parent pid, lowercase executable name) of every process that can be read.
pub fn process_entries() -> Vec<(u32, u32, String)> {
    all_pids()
        .into_iter()
        .filter_map(|pid| {
            let info = bsd_info(pid)?;
            let name = match text(&info.pbi_name) {
                name if name.is_empty() => text(&info.pbi_comm),
                name => name,
            };
            Some((pid, info.pbi_ppid, name.to_lowercase()))
        })
        .collect()
}

/// The command line of `pid` (`KERN_PROCARGS2`: argc, the executable path,
/// padding, then the arguments), when this user may read it.
pub fn process_argv(pid: u32) -> Option<Vec<String>> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as i32];
    let mut size: libc::size_t = 0;
    // SAFETY: a size query with a null buffer.
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) } != 0 || size < 4 {
        return None;
    }
    let mut buffer = vec![0u8; size];
    // SAFETY: `buffer` has `size` bytes.
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buffer.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) } != 0 {
        return None;
    }
    buffer.truncate(size);
    let argc = i32::from_ne_bytes(buffer.get(..4)?.try_into().ok()?).max(0) as usize;
    let rest = &buffer[4..];
    // Skip the executable path and the NULs after it.
    let path_end = rest.iter().position(|&b| b == 0)?;
    let mut at = path_end;
    while rest.get(at) == Some(&0) {
        at += 1;
    }
    let args: Vec<String> = rest[at..].split(|&b| b == 0).take(argc).map(|arg| String::from_utf8_lossy(arg).into_owned()).collect();
    (args.len() == argc).then_some(args)
}

/// Nanoseconds per Mach absolute time unit (1 on Intel, 125/3 on Apple silicon).
fn timebase() -> (u64, u64) {
    static BASE: OnceLock<(u64, u64)> = OnceLock::new();
    *BASE.get_or_init(|| {
        #[repr(C)]
        struct Timebase {
            numer: u32,
            denom: u32,
        }
        unsafe extern "C" {
            fn mach_timebase_info(info: *mut Timebase) -> i32;
        }
        let mut info = Timebase { numer: 0, denom: 0 };
        // SAFETY: a valid out-pointer.
        if unsafe { mach_timebase_info(&mut info) } == 0 && info.denom != 0 { (info.numer as u64, info.denom as u64) } else { (1, 1) }
    })
}

pub fn usage(pid: u32) -> super::Usage {
    let mut info = MaybeUninit::<libc::rusage_info_v2>::zeroed();
    // SAFETY: `info` has room for the RUSAGE_INFO_V2 structure.
    if pid > i32::MAX as u32 || unsafe { libc::proc_pid_rusage(pid as i32, libc::RUSAGE_INFO_V2, info.as_mut_ptr().cast()) } != 0 {
        return super::Usage::default();
    }
    // SAFETY: filled by proc_pid_rusage.
    let info = unsafe { info.assume_init() };
    let (numer, denom) = timebase();
    let ns = (info.ri_user_time + info.ri_system_time) as u128 * numer as u128 / denom as u128;
    super::Usage { working_set: info.ri_resident_size, private: info.ri_phys_footprint, cpu_ms: (ns / 1_000_000) as u64 }
}

pub fn process_uid(pid: u32) -> io::Result<u32> {
    bsd_info(pid).map(|info| info.pbi_uid).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no process {pid}")))
}

/// macOS has no child subreaper: the service remembers terminals' trees
/// instead (MAC-LINUX-PLAN §5.2).
pub fn contain_descendants() -> io::Result<()> {
    Ok(())
}

pub fn raise_self() {
    // SAFETY: sets the calling thread's own QoS class.
    unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0) };
}

pub fn set_priority(pid: u32, class: Class) -> bool {
    let value = if class == Class::Background { libc::PRIO_DARWIN_BG } else { 0 };
    // SAFETY: setpriority on a checked pid; the same user may set and clear the band.
    unsafe { libc::setpriority(libc::PRIO_DARWIN_PROCESS, pid as libc::id_t, value) == 0 }
}
