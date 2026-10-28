//! Window placement: each window's position, size, maximized state and
//! monitor, saved to `<home>/prefs/<client>-window.json` and restored at
//! start; a placement whose monitor is gone (or that would be off screen) is
//! clamped to a visible monitor's work area.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    /// Outer position of the restored (not maximized) window, physical px.
    pub x: i32,
    pub y: i32,
    /// Inner size of the restored window, physical px.
    pub width: u32,
    pub height: u32,
    pub maximized: bool,
    /// The monitor's device name (`\\.\DISPLAY1`).
    pub monitor: Option<String>,
}

/// A monitor's name and work area (x0, y0, x1, y1; physical px).
#[derive(Clone, Debug, PartialEq)]
pub struct Monitor {
    pub name: String,
    pub work: [i32; 4],
    pub primary: bool,
}

pub fn path(home: &Path, client: &str) -> PathBuf {
    home.join("prefs").join(format!("{client}-window.json"))
}

pub fn load(path: &Path) -> Option<Placement> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

pub fn save(path: &Path, placement: &Placement) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&temp, serde_json::to_vec_pretty(placement)?)?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

/// How much of a window's title bar must lie on a monitor to stay there.
const VISIBLE: i32 = 80;

/// The placement moved onto a monitor that exists: kept when its monitor is
/// there and enough of it is visible, else centred on the primary monitor's
/// work area; never larger than the work area.
pub fn clamp(mut p: Placement, monitors: &[Monitor]) -> Placement {
    let Some(fallback) = monitors.iter().find(|m| m.primary).or(monitors.first()) else { return p };
    let visible_on = |m: &Monitor| {
        let [x0, y0, x1, y1] = m.work;
        let w = (p.x + p.width as i32).min(x1) - p.x.max(x0);
        // The title bar (the top 30 px) must be reachable.
        let h = (p.y + 30).min(y1) - p.y.max(y0);
        w >= VISIBLE && h > 0
    };
    let named = p.monitor.as_deref().and_then(|name| monitors.iter().find(|m| m.name == name));
    let home = match named.filter(|m| visible_on(m)) {
        Some(m) => m.clone(),
        None => match monitors.iter().find(|m| visible_on(m)) {
            Some(m) => m.clone(),
            None => {
                let [x0, y0, x1, y1] = fallback.work;
                p.width = p.width.min((x1 - x0) as u32);
                p.height = p.height.min((y1 - y0) as u32);
                p.x = x0 + ((x1 - x0) - p.width as i32) / 2;
                p.y = y0 + ((y1 - y0) - p.height as i32) / 2;
                p.monitor = Some(fallback.name.clone());
                return p;
            }
        },
    };
    let [x0, y0, x1, y1] = home.work;
    p.width = p.width.min((x1 - x0) as u32);
    p.height = p.height.min((y1 - y0) as u32);
    p.x = p.x.clamp(x0, (x1 - p.width as i32).max(x0));
    p.y = p.y.clamp(y0, (y1 - p.height as i32).max(y0));
    p.monitor = Some(home.name);
    p
}

/// The monitors winit knows, with their work areas (taskbar excluded).
pub fn monitors(event_loop: &winit::event_loop::ActiveEventLoop) -> Vec<Monitor> {
    use winit::platform::windows::MonitorHandleExtWindows;
    let primary = event_loop.primary_monitor();
    event_loop
        .available_monitors()
        .map(|m| {
            let (pos, size) = (m.position(), m.size());
            let full = [pos.x, pos.y, pos.x + size.width as i32, pos.y + size.height as i32];
            Monitor { name: m.name().unwrap_or_default(), work: work_area(m.hmonitor()).unwrap_or(full), primary: primary.as_ref() == Some(&m) }
        })
        .collect()
}

fn work_area(hmonitor: isize) -> Option<[i32; 4]> {
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    // SAFETY: a monitor handle from winit and a properly sized out-struct.
    let ok = unsafe { GetMonitorInfoW(hmonitor as _, &mut info) } != 0;
    let r = info.rcWork;
    ok.then_some([r.left, r.top, r.right, r.bottom])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitors() -> Vec<Monitor> {
        vec![
            Monitor { name: r"\\.\DISPLAY1".into(), work: [0, 0, 2560, 1400], primary: true },
            Monitor { name: r"\\.\DISPLAY2".into(), work: [2560, 0, 4480, 1040], primary: false },
        ]
    }

    #[test]
    fn placements_stay_on_a_visible_monitor() {
        let on_second = Placement { x: 2700, y: 100, width: 1500, height: 900, maximized: false, monitor: Some(r"\\.\DISPLAY2".into()) };
        assert_eq!(clamp(on_second.clone(), &monitors()), on_second, "kept where it was");
        // Its monitor is gone: centred on the primary one.
        let gone = clamp(on_second.clone(), &monitors()[..1]);
        assert_eq!((gone.x, gone.y, gone.width, gone.height, gone.monitor.as_deref()), (530, 250, 1500, 900, Some(r"\\.\DISPLAY1")));
        // Far off screen with an unknown monitor.
        let lost = clamp(Placement { x: -40000, y: -40000, width: 5000, height: 3000, maximized: true, monitor: Some(r"\\.\DISPLAY9".into()) }, &monitors());
        assert_eq!((lost.x, lost.y, lost.width, lost.height, lost.maximized), (0, 0, 2560, 1400, true));
        // Partly below the work area: pulled up into it.
        let low = clamp(Placement { x: 100, y: 1300, width: 800, height: 600, maximized: false, monitor: Some(r"\\.\DISPLAY1".into()) }, &monitors());
        assert_eq!((low.x, low.y), (100, 800));
        assert_eq!(clamp(low.clone(), &[]), low, "no monitor information: unchanged");
    }

    #[test]
    fn placements_round_trip() {
        let dir = std::env::temp_dir().join(format!("cc-place-{}", std::process::id()));
        let file = path(&dir, "default");
        let p = Placement { x: 10, y: 20, width: 800, height: 600, maximized: true, monitor: Some("M".into()) };
        save(&file, &p).unwrap();
        assert_eq!(load(&file), Some(p));
        let _ = std::fs::remove_dir_all(dir);
    }
}
