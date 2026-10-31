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

/// Whether a graphical session is reachable from this process (Linux: an
/// X11 or Wayland display; macOS and Windows: always).
pub fn has_display() -> bool {
    match platform() {
        Platform::Linux => ["DISPLAY", "WAYLAND_DISPLAY"].iter().any(|v| std::env::var_os(v).is_some_and(|v| !v.is_empty())),
        _ => true,
    }
}
