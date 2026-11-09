//! The Dock tile and attention (MAC-LINUX-PLAN §5.6, §6.1: the taskbar
//! button's macOS equivalent): while a session needs you the tile shows the
//! count as its badge and an amber dot, while sessions work a blue dot;
//! attention bounces the icon once (`requestUserAttention`). The Dock tile
//! belongs to the app, so every window sets the same one (they show the same
//! canvas). Which window has the focus comes from winit's focus events.

use std::cell::Cell;
use std::sync::Mutex;

use objc2::rc::Retained;
use objc2::{ClassType, DeclaredClass, declare_class, msg_send_id, mutability};
use objc2_app_kit::{NSApplication, NSBezierPath, NSColor, NSRequestUserAttentionType, NSResponder, NSView};
use objc2_foundation::{MainThreadMarker, NSObject, NSPoint, NSRect, NSSize, NSString};

/// OSC 9;4-style states the platform shows (as on Windows): busy, needs you.
const BUSY: u8 = 3;
const NEEDS_YOU: u8 = 4;

declare_class!(
    /// The tile's content: the app's icon with a status dot.
    struct DockView;

    unsafe impl ClassType for DockView {
        #[inherits(NSResponder, NSObject)]
        type Super = NSView;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "AtelierDockView";
    }

    impl DeclaredClass for DockView {
        type Ivars = Cell<u8>;
    }

    unsafe impl DockView {
        #[method(drawRect:)]
        fn draw_rect(&self, _dirty: NSRect) {
            let Some(mtm) = MainThreadMarker::new() else { return };
            let bounds = self.bounds();
            let (w, h) = (bounds.size.width, bounds.size.height);
            let color = match self.ivars().get() {
                BUSY => [0.35, 0.62, 1.0],
                NEEDS_YOU => [1.0, 0.72, 0.18],
                _ => [0.0; 3],
            };
            // SAFETY: AppKit drawing inside drawRect: (a current graphics context).
            unsafe {
                if let Some(icon) = NSApplication::sharedApplication(mtm).applicationIconImage() {
                    icon.drawInRect(bounds);
                }
                if color == [0.0; 3] {
                    return;
                }
                // Bottom right (the view is not flipped: y grows upwards).
                let d = w.min(h) * 0.3;
                let dot = NSRect::new(NSPoint::new(w - d - w * 0.05, h * 0.05), NSSize::new(d, d));
                NSColor::whiteColor().setFill();
                NSBezierPath::bezierPathWithOvalInRect(NSRect::new(NSPoint::new(dot.origin.x - d * 0.08, dot.origin.y - d * 0.08), NSSize::new(d * 1.16, d * 1.16))).fill();
                NSColor::colorWithSRGBRed_green_blue_alpha(color[0], color[1], color[2], 1.0).setFill();
                NSBezierPath::bezierPathWithOvalInRect(dot).fill();
            }
        }
    }
);

impl DockView {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(Cell::new(0));
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(128.0, 128.0));
        // SAFETY: NSView's designated initializer.
        unsafe { msg_send_id![super(this), initWithFrame: frame] }
    }
}

/// The count of sessions that need you, from the window's title
/// (`(2) Atelier — 2 need you`).
fn attention_count(view: isize) -> Option<u32> {
    // SAFETY: the canvas window's live content view, on the main thread.
    let view = unsafe { (view as *const NSView).as_ref() }?;
    let title = view.window()?.title().to_string();
    let rest = title.strip_prefix('(')?;
    rest[..rest.find(')')?].parse().ok()
}

/// The Dock tile (shown by every window: there is one per app).
#[derive(Default)]
pub struct Taskbar {
    pub shown: Option<u8>,
    /// The badge as shown.
    pub badge: Option<String>,
    view: Option<Retained<DockView>>,
}

impl Taskbar {
    pub fn set(&mut self, window: isize, state: u8) {
        if window == 0 {
            return;
        }
        let Some(mtm) = MainThreadMarker::new() else { return };
        let badge = (state == NEEDS_YOU).then(|| attention_count(window).map_or_else(|| "!".to_owned(), |n| n.to_string()));
        if self.shown == Some(state) && self.badge == badge {
            return;
        }
        let tile = NSApplication::sharedApplication(mtm);
        // SAFETY: the app's Dock tile, on the main thread.
        unsafe {
            let tile = tile.dockTile();
            tile.setBadgeLabel(badge.as_deref().map(NSString::from_str).as_deref());
            if matches!(state, BUSY | NEEDS_YOU) {
                let view = self.view.get_or_insert_with(|| DockView::new(mtm));
                view.ivars().set(state);
                view.setNeedsDisplay(true);
                tile.setContentView(Some(view));
            } else {
                tile.setContentView(None);
            }
            tile.display();
        }
        self.shown = Some(state);
        self.badge = badge;
    }
}

/// Which windows (content views) have the keyboard focus.
static FOCUSED: Mutex<Vec<isize>> = Mutex::new(Vec::new());

/// winit's focus news for `in_front` (any thread may ask then).
pub fn track_window(window: &std::sync::Arc<winit::window::Window>, focused: bool) {
    let view = super::native_window(window);
    let mut list = FOCUSED.lock().unwrap_or_else(|p| p.into_inner());
    list.retain(|v| *v != view);
    if focused {
        list.push(view);
    }
}

/// The window is the active one (the app active, the window key).
pub fn in_front(view: isize) -> bool {
    view != 0 && FOCUSED.lock().unwrap_or_else(|p| p.into_inner()).contains(&view)
}

/// Bounces the Dock icon once (until the app comes to the front).
pub fn flash(_view: isize) {
    if let Some(mtm) = MainThreadMarker::new() {
        NSApplication::sharedApplication(mtm).requestUserAttention(NSRequestUserAttentionType::NSInformationalRequest);
    }
}
