//! File drops on Wayland (MAC-LINUX-PLAN §5.6, parity row G3): winit 0.30
//! handles X11's XDND but has no Wayland drag and drop, so a thread of ours
//! takes `wl_data_device` events for the GUI's surfaces on winit's own
//! display connection (its own event queue: libwayland lets several readers
//! share one connection), accepts `text/uri-list` offers, reads the dropped
//! files' paths and hands them to the window, which handles them as winit's
//! `DroppedFile` (pasted quoted in a terminal, a folder on the canvas starts
//! a session there; uploaded in remote mode).

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::AsFd;
use std::sync::{Arc, Mutex, Weak};

use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_data_device::{self, WlDataDevice};
use wayland_client::protocol::wl_data_device_manager::{DndAction, WlDataDeviceManager};
use wayland_client::protocol::wl_data_offer::{self, WlDataOffer};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};
use winit::window::Window;

const URI_LIST: &str = "text/uri-list";

/// A drop that reached a window: its surface, where (surface pixels, before
/// the window's scale) and the files.
struct Dropped {
    surface: usize,
    x: f64,
    y: f64,
    paths: Vec<String>,
}

static DROPS: Mutex<Vec<Dropped>> = Mutex::new(Vec::new());
/// The last drop's place per surface, for the window's `cursor_position`.
static PLACES: Mutex<Vec<(usize, f64, f64)>> = Mutex::new(Vec::new());
static STARTED: std::sync::Once = std::sync::Once::new();

/// Starts the drop thread on the first Wayland window's connection.
pub fn start(window: &Arc<Window>) {
    use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
    let Ok(RawDisplayHandle::Wayland(handle)) = window.display_handle().map(|h| h.as_raw()) else { return };
    let display = handle.display.as_ptr() as usize;
    let window = Arc::downgrade(window);
    STARTED.call_once(move || {
        let _ = std::thread::Builder::new().name("wayland-dnd".into()).spawn(move || {
            if let Err(error) = run(display, window) {
                crate::crashlog::event("dnd", &format!("Wayland drag and drop is off: {error}"));
            }
        });
    });
}

/// The paths dropped on `window` since the last call (and where, for
/// [`place`]).
pub fn take(window: &Window) -> Vec<String> {
    let surface = super::native_window(window) as usize;
    let mut drops = DROPS.lock().unwrap_or_else(|p| p.into_inner());
    let mut paths = Vec::new();
    for drop in drops.extract_if(.., |d| d.surface == surface) {
        let scale = window.scale_factor();
        let mut places = PLACES.lock().unwrap_or_else(|p| p.into_inner());
        places.retain(|(s, _, _)| *s != surface);
        places.push((surface, drop.x * scale, drop.y * scale));
        paths.extend(drop.paths);
    }
    paths
}

/// Where the last drop on `window` landed (window pixels), once.
pub fn place(window: &Window) -> Option<(f32, f32)> {
    let surface = super::native_window(window) as usize;
    let mut places = PLACES.lock().unwrap_or_else(|p| p.into_inner());
    let at = places.iter().position(|(s, _, _)| *s == surface)?;
    let (_, x, y) = places.remove(at);
    Some((x as f32, y as f32))
}

#[derive(Default)]
struct State {
    /// Each live offer's MIME types.
    offers: HashMap<ObjectId, Vec<String>>,
    /// The drag over one of our surfaces: offer, surface, position.
    current: Option<(Option<WlDataOffer>, usize, f64, f64)>,
    conn: Option<Connection>,
    _window: Weak<Window>,
}

fn run(display: usize, window: Weak<Window>) -> Result<(), String> {
    // SAFETY: winit's live display; this connection never closes it.
    let backend = unsafe { Backend::from_foreign_display(display as *mut _) };
    let conn = Connection::from_backend(backend);
    let (globals, mut queue) = registry_queue_init::<State>(&conn).map_err(|e| e.to_string())?;
    let qh = queue.handle();
    let manager: WlDataDeviceManager = globals.bind(&qh, 1..=3, ()).map_err(|e| format!("no data device manager: {e}"))?;
    let seat: WlSeat = globals.bind(&qh, 1..=1, ()).map_err(|e| format!("no seat: {e}"))?;
    let _device = manager.get_data_device(&seat, &qh, ());
    let mut state = State { conn: Some(conn.clone()), _window: window, ..Default::default() };
    loop {
        queue.blocking_dispatch(&mut state).map_err(|e| e.to_string())?;
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &WlRegistry, _: <WlRegistry as Proxy>::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlDataDeviceManager, ()> for State {
    fn event(_: &mut Self, _: &WlDataDeviceManager, _: <WlDataDeviceManager as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlSeat, ()> for State {
    fn event(_: &mut Self, _: &WlSeat, _: <WlSeat as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlDataOffer, ()> for State {
    fn event(state: &mut Self, offer: &WlDataOffer, event: wl_data_offer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            state.offers.entry(offer.id()).or_default().push(mime_type);
        }
    }
}

impl Dispatch<WlDataDevice, ()> for State {
    fn event(state: &mut Self, _: &WlDataDevice, event: wl_data_device::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_data_device::Event::Enter { serial, surface, x, y, id } => {
                let files = id.as_ref().is_some_and(|o| state.offers.get(&o.id()).is_some_and(|m| m.iter().any(|m| m == URI_LIST)));
                if let Some(offer) = id.as_ref() {
                    if files {
                        offer.accept(serial, Some(URI_LIST.into()));
                        if offer.version() >= 3 {
                            offer.set_actions(DndAction::Copy, DndAction::Copy);
                        }
                    } else {
                        offer.accept(serial, None);
                    }
                }
                state.current = Some((id.filter(|_| files), surface.id().as_ptr() as usize, x, y));
            }
            wl_data_device::Event::Motion { x, y, .. } => {
                if let Some(current) = state.current.as_mut() {
                    current.2 = x;
                    current.3 = y;
                }
            }
            wl_data_device::Event::Leave => {
                if let Some((Some(offer), ..)) = state.current.take() {
                    state.offers.remove(&offer.id());
                    offer.destroy();
                }
            }
            wl_data_device::Event::Drop => {
                let Some((Some(offer), surface, x, y)) = state.current.take() else { return };
                let paths = receive(&offer, state.conn.as_ref());
                if offer.version() >= 3 {
                    offer.finish();
                }
                state.offers.remove(&offer.id());
                offer.destroy();
                if !paths.is_empty() {
                    DROPS.lock().unwrap_or_else(|p| p.into_inner()).push(Dropped { surface, x, y, paths });
                    crate::shell::wake_web();
                }
            }
            // A selection offer (the clipboard) is not ours to keep.
            wl_data_device::Event::Selection { id: Some(offer) } => {
                state.offers.remove(&offer.id());
                offer.destroy();
            }
            _ => {}
        }
    }

    event_created_child!(State, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);
}

/// The dropped files: the offer's `text/uri-list` read through a pipe.
fn receive(offer: &WlDataOffer, conn: Option<&Connection>) -> Vec<String> {
    let Ok((mut reader, writer)) = std::io::pipe() else { return Vec::new() };
    offer.receive(URI_LIST.into(), writer.as_fd());
    drop(writer);
    if let Some(conn) = conn {
        let _ = conn.flush();
    }
    let mut text = String::new();
    let _ = reader.read_to_string(&mut text);
    paths(&text)
}

/// Local file paths of a `text/uri-list` (comments and other schemes left out).
fn paths(list: &str) -> Vec<String> {
    list.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|uri| uri.strip_prefix("file://"))
        .map(|rest| rest.strip_prefix("localhost").unwrap_or(rest))
        .filter(|path| path.starts_with('/'))
        .map(percent_decode)
        .collect()
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"), 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    #[test]
    fn uri_lists_become_local_paths() {
        let list = "# from Files\r\nfile:///home/ana/My%20notes.txt\r\nfile://localhost/tmp/a%25b\r\nhttps://example.com/x\r\n\r\n";
        assert_eq!(super::paths(list), ["/home/ana/My notes.txt", "/tmp/a%b"]);
    }
}
