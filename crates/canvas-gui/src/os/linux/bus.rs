//! The session bus (D-Bus): desktop notifications through
//! `org.freedesktop.Notifications` and the dock's launcher entry through
//! `com.canonical.Unity.LauncherEntry` (KDE Plasma, Ubuntu Dock, Dash to Dock
//! show it; plain GNOME shows nothing, the title and urgency remain;
//! MAC-LINUX-PLAN §5.6, §6.1). Without a session bus both are unavailable
//! and say why.

use std::collections::HashMap;
use std::sync::OnceLock;

use zbus::blocking::Connection;
use zbus::zvariant::Value;

/// The `.desktop` file the notifications and the launcher entry belong to.
const DESKTOP: &str = "copilot-canvas";

fn session() -> Result<&'static Connection, String> {
    static BUS: OnceLock<Result<Connection, String>> = OnceLock::new();
    BUS.get_or_init(|| Connection::session().map_err(|e| format!("no session bus: {e}"))).as_ref().map_err(Clone::clone)
}

/// The bus is reachable (the toast thread's start).
pub fn init() -> Result<(), String> {
    session().map(|_| ())
}

/// One desktop notification (`Notify`), attributed to the app's `.desktop` file.
pub fn notify(title: &str, body: &str) -> Result<(), String> {
    let bus = session()?;
    let hints: HashMap<&str, Value> = HashMap::from([("desktop-entry", Value::from(DESKTOP)), ("urgency", Value::from(1u8))]);
    let actions: Vec<&str> = Vec::new();
    bus.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "Notify",
        &("Copilot Canvas", 0u32, DESKTOP, title, body, actions, hints, -1i32),
    )
    .map_err(|e| format!("the notification service refused: {e}"))?;
    Ok(())
}

/// The launcher entry for the taskbar state (3: sessions working, shown as a
/// full progress bar; 4: a session needs you, urgent; else nothing).
pub fn launcher(state: u8) {
    let Ok(bus) = session() else { return };
    let props: HashMap<&str, Value> = HashMap::from([
        ("progress-visible", Value::from(state == 3)),
        ("progress", Value::from(if state == 3 { 1.0f64 } else { 0.0 })),
        ("urgent", Value::from(state == 4)),
    ]);
    let _ = bus.emit_signal(None::<&str>, "/com/canonical/unity/launcherentry/copilot_canvas", "com.canonical.Unity.LauncherEntry", "Update", &(format!("application://{DESKTOP}.desktop"), props));
}
