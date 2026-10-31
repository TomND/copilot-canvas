//! Linux: X11 and Wayland integration (launcher entry, freedesktop
//! notifications, the WebKitGTK report view, work areas) belongs here and to
//! the Linux lead (MAC-LINUX-PLAN §5.6, §5.7); until it lands, stand-ins.

pub use super::unix::{clip_image, native_window, process, since_start, webview};

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
        // Fallbacks cosmic-text picks per character: emoji, symbols, CJK.
        for pattern in ["emoji:color=true", "Noto Color Emoji", "DejaVu Sans", "Noto Sans Symbols2", ":lang=zh-cn", ":lang=ja", ":lang=ko"] {
            if let Some((_, file)) = fc_match(pattern) {
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

pub fn in_front(_window: isize) -> bool {
    false
}

pub fn flash(_window: isize) {}

pub fn toast_thread_init() -> Result<(), String> {
    Err("notifications are not available on this platform yet".into())
}

pub fn show_toast(_title: &str, _body: &str) -> Result<(), String> {
    Err("notifications are not available on this platform yet".into())
}

pub fn toast_history_count() -> Option<u32> {
    None
}

/// Nothing to register outside Windows (the bundle identifier or the
/// `.desktop` file names the app).
pub fn set_app_id() {}

/// The Dock tile or launcher entry (not shown yet).
#[derive(Default)]
pub struct Taskbar {
    pub shown: Option<u8>,
}

impl Taskbar {
    pub fn set(&mut self, window: isize, state: u8) {
        if window != 0 {
            self.shown = Some(state);
        }
    }
}

/// Work areas are the monitors' full areas for now.
pub fn work_area(_monitor: &winit::monitor::MonitorHandle) -> Option<[i32; 4]> {
    None
}

/// The bundle's icon (macOS) or the `.desktop` file's (Linux) is used.
pub fn with_app_icon(attrs: WindowAttributes) -> WindowAttributes {
    attrs
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
