//! Windows: the taskbar button (ITaskbarList3), flashing, WinRT toasts with
//! a registered AppUserModelID, monitor work areas, process counters, the
//! embedded icon, quiet test windows, cursor position and posted keys, the
//! WebView2 report view and clipboard images (moved unchanged from the
//! modules that used them).

pub mod clipimage;
pub mod webview;

use std::time::Duration;

use winit::window::{Window, WindowAttributes};

/// The app id toasts and the taskbar button use.
pub const AUMID: &str = "CopilotCanvas.Gui";

/// The window is the foreground window (no toast, no flashing).
pub fn in_front(hwnd: isize) -> bool {
    // SAFETY: no arguments.
    hwnd != 0 && unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() } as isize == hwnd
}

/// Flashes the taskbar button three times (until the window comes to the front).
pub fn flash(hwnd: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{FLASHW_TIMERNOFG, FLASHW_TRAY, FLASHWINFO, FlashWindowEx};
    let info = FLASHWINFO { cbSize: std::mem::size_of::<FLASHWINFO>() as u32, hwnd: hwnd as _, dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG, uCount: 3, dwTimeout: 0 };
    // SAFETY: a valid FLASHWINFO for this process's window.
    unsafe { FlashWindowEx(&info) };
}

/// Prepares the toast thread: COM in the multithreaded apartment and the
/// registered app id.
pub fn toast_thread_init() -> Result<(), String> {
    // SAFETY: initializes COM for this thread only.
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
    }
    register_aumid()
}

/// Shows one toast (on the toast thread).
pub fn show_toast(title: &str, body: &str) -> Result<(), String> {
    show_winrt_toast(title, body).map_err(|e| e.message().to_string())
}

/// Toasts of this app in the Action Center.
pub fn toast_history_count() -> Option<u32> {
    history_count().ok()
}

/// The work area (taskbar excluded) of a monitor.
pub fn work_area(monitor: &winit::monitor::MonitorHandle) -> Option<[i32; 4]> {
    use winit::platform::windows::MonitorHandleExtWindows;
    work_area_of(monitor.hmonitor())
}

/// The app icon embedded by build.rs: small in the title bar, large on the
/// taskbar button and in Alt+Tab.
pub fn with_app_icon(attrs: WindowAttributes) -> WindowAttributes {
    use winit::dpi::PhysicalSize;
    use winit::platform::windows::{IconExtWindows, WindowAttributesExtWindows};
    let icon = |px: u32| winit::window::Icon::from_resource(1, Some(PhysicalSize::new(px, px))).ok();
    attrs.with_window_icon(icon(32)).with_taskbar_icon(icon(256))
}

/// A test run on someone's desktop: never take the focus, stay out of the
/// taskbar and behind other windows (input comes from the hooks).
pub fn quiet_window(attrs: WindowAttributes) -> WindowAttributes {
    use winit::platform::windows::WindowAttributesExtWindows;
    attrs.with_active(false).with_skip_taskbar(true).with_window_level(winit::window::WindowLevel::AlwaysOnBottom)
}

/// An image on the clipboard (PNG, DIBV5 or DIB) as RGBA.
pub fn clip_image() -> Option<(u32, u32, Vec<u8>)> {
    clipimage::read()
}

fn history_count() -> windows::core::Result<u32> {
    use windows::UI::Notifications::ToastNotificationManager;
    ToastNotificationManager::History()?.GetHistoryWithId(&windows::core::HSTRING::from(AUMID))?.Size()
}

fn show_winrt_toast(title: &str, body: &str) -> windows::core::Result<()> {
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
    use windows::core::HSTRING;
    let xml = XmlDocument::new()?;
    xml.LoadXml(&HSTRING::from(format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        xml_escape(title),
        xml_escape(body)
    )))?;
    let toast = ToastNotification::CreateToastNotification(&xml)?;
    toast.SetGroup(&HSTRING::from("copilot-canvas"))?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?.Show(&toast)
}

fn xml_escape(text: &str) -> String {
    text.chars().take(400).fold(String::new(), |mut out, c| {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
        out
    })
}

/// Registers [`AUMID`] for toasts (per user; idempotent).
fn register_aumid() -> Result<(), String> {
    use windows_sys::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegSetValueExW};
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let path = wide(&format!(r"Software\Classes\AppUserModelId\{AUMID}"));
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: valid NUL-terminated strings; the key is closed below.
    let status = unsafe { RegCreateKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, std::ptr::null(), REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, std::ptr::null(), &mut key, std::ptr::null_mut()) };
    if status != 0 {
        return Err(format!("cannot register the app for toasts (error {status})"));
    }
    let name = wide("Copilot Canvas");
    let value = wide("DisplayName");
    // SAFETY: `name` is a NUL-terminated UTF-16 string of the given byte length.
    let status = unsafe { RegSetValueExW(key, value.as_ptr(), 0, REG_SZ, name.as_ptr().cast(), (name.len() * 2) as u32) };
    // SAFETY: opened above.
    unsafe { RegCloseKey(key) };
    if status != 0 { Err(format!("cannot register the app for toasts (error {status})")) } else { Ok(()) }
}

/// Gives the process the app id toasts are registered under (call before
/// the first window, so the taskbar button carries it too).
pub fn set_app_id() {
    // SAFETY: a static NUL-terminated string.
    let _ = unsafe { windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(windows::core::w!("CopilotCanvas.Gui")) };
}

/// The taskbar button's progress indicator.
#[derive(Default)]
pub struct Taskbar {
    list: Option<windows::Win32::UI::Shell::ITaskbarList3>,
    pub shown: Option<u8>,
}

impl Taskbar {
    pub fn set(&mut self, hwnd: isize, state: u8) {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
        use windows::Win32::UI::Shell::{ITaskbarList3, TBPF_INDETERMINATE, TBPF_NOPROGRESS, TBPF_PAUSED, TaskbarList};
        if hwnd == 0 || self.shown == Some(state) {
            return;
        }
        if self.list.is_none() {
            // SAFETY: COM on the UI thread (already initialized by winit; this is a no-op then).
            let list: Option<ITaskbarList3> = unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER).ok()
            };
            // SAFETY: HrInit before first use, as documented.
            self.list = list.filter(|l| unsafe { l.HrInit() }.is_ok());
        }
        let Some(list) = &self.list else { return };
        let hwnd = HWND(hwnd as *mut core::ffi::c_void);
        // SAFETY: a valid taskbar list and this process's window.
        unsafe {
            let _ = match state {
                3 => list.SetProgressState(hwnd, TBPF_INDETERMINATE),
                4 => list.SetProgressState(hwnd, TBPF_PAUSED).and_then(|_| list.SetProgressValue(hwnd, 100, 100)),
                _ => list.SetProgressState(hwnd, TBPF_NOPROGRESS),
            };
        }
        self.shown = Some(state);
    }
}

fn work_area_of(hmonitor: isize) -> Option<[i32; 4]> {
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    // SAFETY: a monitor handle from winit and a properly sized out-struct.
    let ok = unsafe { GetMonitorInfoW(hmonitor as _, &mut info) } != 0;
    let r = info.rcWork;
    ok.then_some([r.left, r.top, r.right, r.bottom])
}

/// Process CPU time and private memory (Win32).
pub fn process() -> (Duration, u64) {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    let mut memory: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    memory.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    // SAFETY: the current process's pseudo handle and properly sized out-structs.
    unsafe {
        GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user);
        GetProcessMemoryInfo(GetCurrentProcess(), &mut memory as *mut _ as *mut _, memory.cb);
    }
    let ticks = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
    (Duration::from_nanos((ticks(kernel) + ticks(user)) * 100), memory.PrivateUsage as u64)
}

/// Time since this process was created.
pub fn since_start() -> Duration {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user, mut now) = (zero, zero, zero, zero, zero);
    // SAFETY: as above.
    unsafe {
        GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user);
        GetSystemTimeAsFileTime(&mut now);
    }
    let ticks = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
    Duration::from_nanos(ticks(now).saturating_sub(ticks(created)) * 100)
}

pub fn native_window(window: &Window) -> isize {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
        _ => 0,
    }
}

/// The mouse pointer's position in a window's client area, in pixels.
pub fn cursor_position(window: &Window) -> Option<(f32, f32)> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut point = POINT { x: 0, y: 0 };
    // SAFETY: plain Win32 calls on this process's window.
    let ok = unsafe { GetCursorPos(&mut point) != 0 && ScreenToClient(native_window(window) as _, &mut point) != 0 };
    ok.then_some((point.x as f32, point.y as f32))
}

/// Posts a key press for an ASCII character to the window (as the OS would).
pub fn post_key(hwnd: isize, c: char) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{MAPVK_VK_TO_VSC, MapVirtualKeyW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_KEYDOWN, WM_KEYUP};
    let vk = match c {
        '\r' | '\n' => 0x0d,
        '\u{8}' => 0x08,
        c => c.to_ascii_uppercase() as u32,
    };
    // SAFETY: posting messages to this process's own window.
    unsafe {
        let scan = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC);
        let down = 1isize | ((scan as isize) << 16);
        PostMessageW(hwnd as _, WM_KEYDOWN, vk as usize, down);
        std::thread::sleep(Duration::from_millis(10));
        PostMessageW(hwnd as _, WM_KEYUP, vk as usize, down | (1 << 30) | (1 << 31));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn toast_text_is_escaped() {
        assert_eq!(super::xml_escape("a<b>&\"c'\n"), "a&lt;b&gt;&amp;&quot;c&apos; ");
    }
}
