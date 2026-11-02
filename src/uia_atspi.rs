//! The accessibility check's outside client on Linux: AT-SPI over D-Bus, as
//! Orca reads the GUI (MAC-LINUX-PLAN §7.1). It turns assistive technology
//! on for the session the way a screen reader does (`org.a11y.Status
//! IsEnabled`, which AccessKit waits for; `restore` turns it back off when it
//! was off), finds the GUI's application on the accessibility bus by its
//! process, walks its tree and presses a button through the Action
//! interface. The snapshot has the UIA client's shape: roles get UIA's
//! control type names where the check looks for them (push button → Button,
//! tree item → TreeItem, terminal → Document, page tab → TabItem), other
//! roles keep their AT-SPI name; a terminal's value is its text from the
//! Text interface, `selected` the SELECTED state.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use zbus::blocking::Connection;
use zbus::zvariant::{DynamicType, OwnedObjectPath, OwnedValue};

/// The client's name in the check's report.
pub const CLIENT: &str = "AT-SPI";

const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const ROOT: &str = "/org/a11y/atspi/accessible/root";
/// The GUI has this long to show up on the accessibility bus.
const REGISTER: Duration = Duration::from_secs(20);

/// The session's assistive-technology switch before the first call.
static WAS_ENABLED: OnceLock<bool> = OnceLock::new();

fn call<B, R>(bus: &Connection, dest: &str, path: &str, iface: &str, method: &str, body: &B) -> Result<R>
where
    B: serde::Serialize + DynamicType,
    R: DeserializeOwned + zbus::zvariant::Type,
{
    let reply = bus.call_method(Some(dest), path, Some(iface), method, body).with_context(|| format!("{iface}.{method} on {dest} {path}"))?;
    Ok(reply.body().deserialize::<R>()?)
}

fn session() -> Result<Connection> {
    Connection::session().context("no session bus (AT-SPI needs one)")
}

/// Assistive technology on, then the accessibility bus.
fn a11y_bus() -> Result<Connection> {
    let session = session()?;
    let get = |name: &str| -> Result<bool> {
        let v: OwnedValue = call(&session, "org.a11y.Bus", "/org/a11y/bus", "org.freedesktop.DBus.Properties", "Get", &("org.a11y.Status", name))?;
        Ok(bool::try_from(v)?)
    };
    let enabled = get("IsEnabled").context("the AT-SPI bus launcher (at-spi2-core)")?;
    WAS_ENABLED.get_or_init(|| enabled);
    if !enabled {
        let _: () = call(&session, "org.a11y.Bus", "/org/a11y/bus", "org.freedesktop.DBus.Properties", "Set", &("org.a11y.Status", "IsEnabled", zbus::zvariant::Value::from(true)))?;
    }
    let address: String = call(&session, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus", "GetAddress", &())?;
    Ok(zbus::blocking::connection::Builder::address(address.as_str())?.build()?)
}

/// Assistive technology back off if it was off before the check (GNOME keeps
/// the switch as a setting).
pub fn restore() {
    if WAS_ENABLED.get() == Some(&false)
        && let Ok(session) = session()
    {
        let _: Result<()> = call(&session, "org.a11y.Bus", "/org/a11y/bus", "org.freedesktop.DBus.Properties", "Set", &("org.a11y.Status", "IsEnabled", zbus::zvariant::Value::from(false)));
    }
}

/// An accessible object: its bus name and path.
type Object = (String, OwnedObjectPath);

/// The application root the process `pid` registered, waiting for it.
fn app_of(bus: &Connection, pid: u32) -> Result<Object> {
    let deadline = Instant::now() + REGISTER;
    loop {
        let apps: Vec<Object> = call(bus, "org.a11y.atspi.Registry", ROOT, ACCESSIBLE, "GetChildren", &())?;
        for app in apps {
            let owner: Result<u32> = call(bus, "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "GetConnectionUnixProcessID", &(app.0.as_str(),));
            if owner.is_ok_and(|p| p == pid) {
                // Registered; its window appears with the first tree.
                let windows: Vec<Object> = call(bus, &app.0, app.1.as_str(), ACCESSIBLE, "GetChildren", &())?;
                if !windows.is_empty() {
                    return Ok(app);
                }
            }
        }
        if Instant::now() > deadline {
            bail!("process {pid} did not register with the AT-SPI registry in {} s", REGISTER.as_secs());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

struct Found {
    object: Object,
    role: String,
    name: String,
    description: String,
    interfaces: Vec<String>,
    selected: bool,
}

/// AT-SPI's `ATSPI_STATE_SELECTED` (a bit of `GetState`'s first word).
const STATE_SELECTED: u32 = 23;

fn describe(bus: &Connection, object: &Object) -> Result<Found> {
    let (dest, path) = (object.0.as_str(), object.1.as_str());
    let props: HashMap<String, OwnedValue> = call(bus, dest, path, "org.freedesktop.DBus.Properties", "GetAll", &(ACCESSIBLE,))?;
    let text = |key: &str| props.get(key).and_then(|v| String::try_from(v.try_clone().ok()?).ok()).unwrap_or_default();
    Ok(Found {
        role: role_name(call(bus, dest, path, ACCESSIBLE, "GetRole", &())?),
        name: text("Name"),
        description: text("Description"),
        interfaces: call(bus, dest, path, ACCESSIBLE, "GetInterfaces", &())?,
        selected: call::<_, Vec<u32>>(bus, dest, path, ACCESSIBLE, "GetState", &()).is_ok_and(|words| words.first().is_some_and(|w| w & (1 << STATE_SELECTED) != 0)),
        object: object.clone(),
    })
}

/// Every object under the application root, depth first.
fn walk(bus: &Connection, app: &Object) -> Result<Vec<Found>> {
    let mut out = Vec::new();
    let mut stack: Vec<Object> = call(bus, &app.0, app.1.as_str(), ACCESSIBLE, "GetChildren", &())?;
    stack.reverse();
    while let Some(object) = stack.pop() {
        let mut children: Vec<Object> = call(bus, &object.0, object.1.as_str(), ACCESSIBLE, "GetChildren", &())?;
        children.reverse();
        stack.extend(children);
        out.push(describe(bus, &object)?);
        if out.len() > 5000 {
            bail!("more than 5000 accessible objects under {}", app.0);
        }
    }
    Ok(out)
}

/// AT-SPI's name for a role number (`GetRole`; AccessKit has no
/// `GetRoleName`).
fn role_name(role: u32) -> String {
    match role {
        2 => "alert",
        6 => "canvas",
        7 => "check box",
        16 => "dialog",
        20 => "filler",
        23 => "frame",
        29 => "label",
        31 => "list",
        32 => "list item",
        33 => "menu",
        37 => "page tab",
        38 => "page tab list",
        39 => "panel",
        43 => "push button",
        54 => "status bar",
        60 => "terminal",
        61 => "text",
        62 => "toggle button",
        63 => "tool bar",
        65 => "tree",
        69 => "window",
        75 => "application",
        79 => "entry",
        82 => "document frame",
        85 => "section",
        91 => "tree item",
        94 => "document text",
        99 => "grouping",
        other => return format!("role {other}"),
    }
    .to_owned()
}

/// UIA's control type names for the roles the check looks for.
fn kind(role: &str) -> &str {
    match role {
        "push button" => "Button",
        "tree item" => "TreeItem",
        "page tab" => "TabItem",
        "terminal" | "document frame" | "document text" => "Document",
        other => other,
    }
}

/// The GUI's accessible objects as an AT-SPI client sees them; `pid` is the
/// GUI's process (UIA finds a window by its handle, AT-SPI an application).
pub fn uia_snapshot(pid: isize) -> Result<Value> {
    let bus = a11y_bus()?;
    let app = app_of(&bus, u32::try_from(pid)?)?;
    let found = walk(&bus, &app)?;
    let mut nodes = Vec::new();
    for f in &found {
        let value = if f.interfaces.iter().any(|i| i == "org.a11y.atspi.Text") {
            call::<_, String>(&bus, &f.object.0, f.object.1.as_str(), "org.a11y.atspi.Text", "GetText", &(0i32, -1i32)).ok()
        } else {
            None
        };
        nodes.push(json!({ "type": kind(&f.role), "role": f.role, "name": f.name, "description": f.description, "value": value, "selected": f.selected, "interfaces": f.interfaces }));
    }
    let window = found.iter().find(|f| f.role == "frame" || f.role == "window").map(|f| f.name.clone()).unwrap_or_default();
    Ok(json!({ "window": window, "count": nodes.len(), "nodes": nodes, "client": CLIENT }))
}

/// Presses the button or tab named `name` through its AT-SPI action
/// (AccessKit's click, as UIA's Invoke and a tab's Select).
pub fn uia_invoke(pid: isize, name: &str) -> Result<()> {
    let bus = a11y_bus()?;
    let app = app_of(&bus, u32::try_from(pid)?)?;
    let pressable = |f: &Found| matches!(f.role.as_str(), "push button" | "page tab") && f.name.trim() == name;
    // The tree may still be changing (a dialog just closed): looked for again for a few seconds.
    let deadline = Instant::now() + Duration::from_secs(5);
    let button = loop {
        let found = walk(&bus, &app)?;
        if let Some(button) = found.iter().find(|f| pressable(f) && f.interfaces.iter().any(|i| i == "org.a11y.atspi.Action")) {
            break button.object.clone();
        }
        if Instant::now() > deadline {
            let seen: Vec<String> = found.iter().filter(|f| f.name.trim() == name).map(|f| format!("{} {:?} {:?}", f.role, f.name, f.interfaces)).collect();
            bail!("no button or tab {name:?} with an action (named so: {})", if seen.is_empty() { "none".to_owned() } else { seen.join("; ") });
        }
        std::thread::sleep(Duration::from_millis(300));
    };
    let done: bool = call(&bus, &button.0, button.1.as_str(), "org.a11y.atspi.Action", "DoAction", &(0i32,))?;
    anyhow::ensure!(done, "the button {name:?} refused its action");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{kind, role_name};

    #[test]
    fn roles_get_uia_names_where_the_check_looks() {
        assert_eq!(role_name(43), "push button");
        assert_eq!(kind(&role_name(43)), "Button");
        assert_eq!(kind(&role_name(91)), "TreeItem");
        assert_eq!(kind(&role_name(60)), "Document");
        assert_eq!(kind(&role_name(16)), "dialog");
        assert_eq!(kind(&role_name(37)), "TabItem");
        assert_eq!(role_name(1000), "role 1000");
    }
}
