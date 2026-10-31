//! Facts about this machine and session.

/// The operating systems the desktop app runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Windows,
    MacOs,
    Linux,
}

impl Platform {
    /// `windows`, `macos` or `linux` (`BackendInfo::platform`).
    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Windows => "windows",
            Platform::MacOs => "macos",
            Platform::Linux => "linux",
        }
    }
}

/// The platform this build runs on.
pub fn platform() -> Platform {
    match std::env::consts::OS {
        "windows" => Platform::Windows,
        "macos" => Platform::MacOs,
        _ => Platform::Linux,
    }
}

/// `name` with this platform's executable suffix (`copilot.exe` on Windows).
pub fn exe_name(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

#[cfg(windows)]
pub use canvas_common::remote::machine_name;

/// This machine's name (what a service lists for a client it paired):
/// macOS's computer name, else the host name.
#[cfg(unix)]
pub fn machine_name() -> String {
    let scutil = (platform() == Platform::MacOs)
        .then(|| std::process::Command::new("/usr/sbin/scutil").args(["--get", "ComputerName"]).stderr(std::process::Stdio::null()).output().ok())
        .flatten()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|name| !name.is_empty());
    scutil.unwrap_or_else(|| {
        let mut buffer = [0u8; 256];
        // SAFETY: `buffer` has the length given.
        let ok = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0;
        let name = String::from_utf8_lossy(&buffer[..buffer.iter().position(|&b| b == 0).unwrap_or(0)]).into_owned();
        if ok && !name.trim().is_empty() { name.trim().trim_end_matches(".local").to_owned() } else { "client".into() }
    })
}

/// The signed-in user's account name.
pub fn user_name() -> String {
    #[cfg(unix)]
    {
        // SAFETY: getpwuid returns a pointer into static storage or null.
        let name = unsafe {
            let entry = libc::getpwuid(libc::geteuid());
            (!entry.is_null() && !(*entry).pw_name.is_null()).then(|| std::ffi::CStr::from_ptr((*entry).pw_name).to_string_lossy().into_owned())
        };
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            return name;
        }
    }
    std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" }).ok().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "user".into())
}

/// A session reached over SSH (Unix), where a GUI client would not be seen.
/// Windows' remote desktop check (`SM_REMOTESESSION`) stays in the CLI.
pub fn ssh_session() -> bool {
    ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"].iter().any(|v| std::env::var_os(v).is_some_and(|v| !v.is_empty()))
}

/// A session where the GPU client is the wrong choice: a remote desktop
/// (Windows `SM_REMOTESESSION`), SSH, or (Linux) no display.
pub fn remote_session() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_REMOTESESSION};
        // SAFETY: a plain query.
        let rdp = unsafe { GetSystemMetrics(SM_REMOTESESSION) } != 0;
        rdp || std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_CLIENT").is_some()
    }
    #[cfg(not(windows))]
    {
        ssh_session() || !has_display()
    }
}

/// Whether a graphical session is reachable from this process (Linux: an
/// X11 or Wayland display; macOS and Windows: always).
pub fn has_display() -> bool {
    match platform() {
        Platform::Linux => ["DISPLAY", "WAYLAND_DISPLAY"].iter().any(|v| std::env::var_os(v).is_some_and(|v| !v.is_empty())),
        _ => true,
    }
}

/// Windows' "Show animations in Windows" (SPI_GETCLIENTAREAANIMATION).
#[cfg(windows)]
pub fn system_animations() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SPI_GETCLIENTAREAANIMATION, SystemParametersInfoW};
    let mut on: i32 = 1;
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL to the pointer.
    let ok = unsafe { SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&mut on as *mut i32).cast(), 0) };
    ok == 0 || on != 0
}

/// Whether the system wants animations: macOS "Reduce motion" off
/// (Accessibility › Display), GNOME's `enable-animations`. Asked at most every
/// 5 s (it runs a small program).
#[cfg(unix)]
pub fn system_animations() -> bool {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static CACHE: Mutex<Option<(Instant, bool)>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, value)) = *cache
        && at.elapsed() < Duration::from_secs(5)
    {
        return value;
    }
    let read = |program: &str, args: &[&str]| {
        std::process::Command::new(program).args(args).stderr(std::process::Stdio::null()).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    let value = match platform() {
        Platform::MacOs => read("/usr/bin/defaults", &["read", "com.apple.universalaccess", "reduceMotion"]).is_none_or(|v| v != "1"),
        _ => read("gsettings", &["get", "org.gnome.desktop.interface", "enable-animations"]).is_none_or(|v| v != "false"),
    };
    *cache = Some((Instant::now(), value));
    value
}

/// The current local UTC offset in minutes.
#[cfg(windows)]
pub fn local_offset_minutes() -> i32 {
    // SAFETY: plain out-parameter calls.
    let (local, utc) = unsafe {
        let mut l = std::mem::zeroed();
        let mut u = std::mem::zeroed();
        windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut l);
        windows_sys::Win32::System::SystemInformation::GetSystemTime(&mut u);
        (l, u)
    };
    let minutes = |t: &windows_sys::Win32::Foundation::SYSTEMTIME| t.wHour as i32 * 60 + t.wMinute as i32;
    let mut diff = minutes(&local) - minutes(&utc);
    if local.wDay != utc.wDay {
        diff += if (local.wYear, local.wMonth, local.wDay) > (utc.wYear, utc.wMonth, utc.wDay) { 1440 } else { -1440 };
    }
    diff
}

/// The current local UTC offset in minutes (`tm_gmtoff`).
#[cfg(unix)]
pub fn local_offset_minutes() -> i32 {
    // SAFETY: time and localtime_r with valid out-pointers.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return 0;
        }
        (tm.tm_gmtoff / 60) as i32
    }
}
