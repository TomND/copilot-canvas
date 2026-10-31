//! Linux: X11 and Wayland integration (launcher entry, freedesktop
//! notifications, the WebKitGTK report view, work areas) belongs here and to
//! the Linux lead (MAC-LINUX-PLAN §5.6, §5.7); until it lands, stand-ins.

pub use super::unix::{clip_image, native_window, process, since_start};

mod bus;
mod gtk;
mod html_helper;
pub mod webview;

/// The GUI binary runs as the HTML view's helper when started for it
/// (`COPILOT_CANVAS_HTML_VIEW=1`): answers its exit code.
pub fn helper() -> Option<i32> {
    std::env::var_os(html_helper::ENV).filter(|v| v == "1").map(|_| html_helper::main())
}

/// wgpu's backends: Vulkan, GL as the fallback (llvmpipe and lavapipe are
/// the software adapters).
pub const GPU_BACKENDS: wgpu::Backends = wgpu::Backends::VULKAN.union(wgpu::Backends::GL);

/// Fonts: named files found with fontconfig (`fc-match`), as Windows and
/// macOS load named files instead of scanning every font (MAC-LINUX-PLAN
/// §5.6): Cascadia Mono when installed, else fontconfig's `monospace`;
/// labels in its `sans-serif` (Cantarell, Noto Sans, DejaVu Sans); colour
/// emoji (Noto Color Emoji, CBDT, which swash draws), CJK and symbol
/// fallbacks. Without `fc-match` the usual font folders are scanned.
pub mod fonts {
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    pub use crate::os::font_sources::FontSources;

    /// `(family, file)` of fontconfig's best match for `pattern`.
    fn fc_match(pattern: &str) -> Option<(String, PathBuf)> {
        let out = Command::new("fc-match").args(["-f", "%{family[0]}\\n%{file}", pattern]).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let (family, file) = text.split_once('\n')?;
        let file = PathBuf::from(file.trim());
        (out.status.success() && file.is_file()).then(|| (family.trim().to_owned(), file))
    }

    /// The styles a grid and labels use, of `family` (regular, bold, italic).
    fn faces(family: &str) -> Vec<PathBuf> {
        [":style=Regular", ":style=Bold", ":style=Italic", ":style=Bold Italic"]
            .iter()
            .filter_map(|style| fc_match(&format!("{family}{style}")).filter(|(f, _)| f == family).map(|(_, file)| file))
            .collect()
    }

    fn leak(names: Vec<String>) -> &'static [&'static str] {
        Box::leak(names.into_iter().map(|n| &*Box::leak(n.into_boxed_str())).collect::<Vec<&'static str>>().into_boxed_slice())
    }

    pub fn sources() -> FontSources {
        const MONO: [&str; 4] = ["Cascadia Mono", "DejaVu Sans Mono", "Noto Sans Mono", "Liberation Mono"];
        const UI: [&str; 3] = ["Cantarell", "Noto Sans", "DejaVu Sans"];
        if fc_match("monospace").is_none() {
            let mut dirs = vec![PathBuf::from("/usr/share/fonts"), PathBuf::from("/usr/local/share/fonts")];
            if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
                dirs.extend([home.join(".local/share/fonts"), home.join(".fonts")]);
            }
            return FontSources { files: Vec::new(), dirs, mono: &MONO, ui: &UI };
        }
        let mut files = Vec::new();
        let mut mono = Vec::new();
        // Cascadia Mono only when it is really installed (fc-match always answers something).
        if let Some((family, _)) = fc_match("Cascadia Mono").filter(|(f, _)| f == "Cascadia Mono") {
            files.extend(faces(&family));
            mono.push(family);
        }
        if let Some((family, _)) = fc_match("monospace") {
            files.extend(faces(&family));
            mono.push(family);
        }
        let mut ui = Vec::new();
        if let Some((family, _)) = fc_match("sans-serif") {
            files.extend(faces(&family));
            ui.push(family);
        }
        // Fallbacks cosmic-text picks per character: emoji, symbols, CJK, and
        // fonts with the canvas's own symbols that DejaVu lacks (⧉ ⏻ ⟲ ⟳ ⋯ ◌ ⌁ ✳ ✎ ❯ ⇢ ⇩).
        let symbols = ["29c9", "23fb", "27f2", "27f3", "22ef", "25cc", "2301", "2733", "270e", "276f", "21e2", "21e9"].map(|c| format!(":charset={c}"));
        let patterns = ["emoji:color=true", "Noto Color Emoji", "DejaVu Sans", "Noto Sans Symbols2", "Noto Sans Math", ":lang=zh-cn", ":lang=ja", ":lang=ko"];
        for pattern in patterns.iter().map(|p| p.to_string()).chain(symbols) {
            if let Some((_, file)) = fc_match(&pattern) {
                files.push(file);
            }
        }
        files.sort();
        files.dedup();
        mono.extend(MONO.iter().map(|s| s.to_string()));
        ui.extend(UI.iter().map(|s| s.to_string()));
        mono.dedup();
        ui.dedup();
        FontSources { files, dirs: Vec::new(), mono: leak(mono), ui: leak(ui) }
    }
}

use winit::window::{Window, WindowAttributes};

/// Which modifier is the app's (MAC-LINUX-PLAN D8).
pub const KEY_POLICY: canvas_core::input::KeyPolicy = canvas_core::input::KeyPolicy::CtrlApp;

/// Nothing to set up before the event loop yet.
pub fn configure_event_loop(_builder: &mut winit::event_loop::EventLoopBuilder<crate::shell::UserEvent>, _quiet_tests: bool) {}

pub fn in_front(_window: isize) -> bool {
    false
}

pub fn flash(_window: isize) {}

/// Notifications go to `org.freedesktop.Notifications` on the session bus.
pub fn toast_thread_init() -> Result<(), String> {
    bus::init()
}

pub fn show_toast(title: &str, body: &str) -> Result<(), String> {
    bus::notify(title, body)
}

/// Linux keeps no notification history an app can read (the tests watch the
/// bus instead, MAC-LINUX-PLAN §6.1).
pub fn toast_history_count() -> Option<u32> {
    None
}

/// Nothing to register outside Windows (the bundle identifier or the
/// `.desktop` file names the app).
pub fn set_app_id() {}

/// The dock's launcher entry (`LauncherEntry` on the session bus).
#[derive(Default)]
pub struct Taskbar {
    pub shown: Option<u8>,
}

impl Taskbar {
    pub fn set(&mut self, window: isize, state: u8) {
        if window == 0 || self.shown == Some(state) {
            return;
        }
        bus::launcher(state);
        self.shown = Some(state);
    }
}

/// Work areas are the monitors' full areas for now.
pub fn work_area(_monitor: &winit::monitor::MonitorHandle) -> Option<[i32; 4]> {
    None
}

/// The app's identity for docks, launchers and `LauncherEntry`: the Wayland
/// `app_id` and the X11 `WM_CLASS` are `copilot-canvas` (the `.desktop`
/// file's name and `StartupWMClass`); X11 also gets the window icon
/// (Wayland compositors take the `.desktop` file's).
pub fn window_attributes(attrs: WindowAttributes) -> WindowAttributes {
    use winit::platform::wayland::WindowAttributesExtWayland;
    use winit::platform::x11::WindowAttributesExtX11;
    let attrs = WindowAttributesExtWayland::with_name(attrs, APP_ID, APP_ID);
    let attrs = WindowAttributesExtX11::with_name(attrs, APP_ID, APP_ID);
    match icon() {
        Some(icon) => attrs.with_window_icon(Some(icon)),
        None => attrs,
    }
}

/// The `.desktop` file's name, the Wayland app id and the X11 class.
pub const APP_ID: &str = "copilot-canvas";

fn icon() -> Option<winit::window::Icon> {
    const PNG: &[u8] = include_bytes!("../../../../../assets/icon/copilot-canvas.png");
    let (width, height, rgba) = canvas_common::clipimage::decode_png(PNG).ok()?;
    winit::window::Icon::from_rgba(rgba, width, height).ok()
}

/// A test run on someone's desktop: never take the focus.
pub fn quiet_window(attrs: WindowAttributes) -> WindowAttributes {
    attrs.with_active(false)
}

/// The pointer's position comes from winit's cursor events instead.
pub fn cursor_position(_window: &Window) -> Option<(f32, f32)> {
    None
}

/// Posted keys (latency benches) are not available yet.
pub fn post_key(_window: isize, _c: char) {}
