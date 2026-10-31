//! GTK 3, WebKitGTK 4.1 and their libraries loaded at run time (`dlopen`):
//! the HTML view's helper needs them, the GUI does not, and a machine
//! without WebKitGTK still runs the GUI (reports then show as formatted
//! text). Only the few functions the helper calls are declared.

#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_int, c_uint, c_void};

use libloading::Library;

pub type gboolean = c_int;
pub type GType = usize;
pub type gpointer = *mut c_void;
pub type GCallback = *const c_void;

/// `GError`'s layout (domain, code, message).
#[repr(C)]
pub struct GError {
    pub domain: u32,
    pub code: c_int,
    pub message: *const c_char,
}

macro_rules! library {
    ($name:ident { $($lib:ident: [$($file:literal),+] { $($fun:ident: $ty:ty;)* })* }) => {
        pub struct $name {
            $(#[allow(dead_code)] $lib: Library,)*
            $($(pub $fun: $ty,)*)*
        }

        impl $name {
            /// Loads every library and symbol; the first missing one is the error.
            pub fn load() -> Result<Self, String> {
                // SAFETY: well-known system libraries whose initializers are safe to run.
                unsafe {
                    $(
                        let $lib = [$($file),+].iter().find_map(|f| Library::new(f).ok()).ok_or_else(|| format!("{} is not installed", [$($file),+][0]))?;
                        $(
                            let $fun: $ty = *$lib.get::<$ty>(concat!(stringify!($fun), "\0").as_bytes()).map_err(|e| format!("{}: {e}", stringify!($fun)))?;
                        )*
                    )*
                    Ok($name { $($lib,)* $($($fun,)*)* })
                }
            }
        }
    };
}

library!(Gtk {
    glib: ["libglib-2.0.so.0"] {
        g_main_context_iteration: unsafe extern "C" fn(gpointer, gboolean) -> gboolean;
        g_main_context_wakeup: unsafe extern "C" fn(gpointer);
        g_bytes_new: unsafe extern "C" fn(*const c_void, usize) -> gpointer;
        g_bytes_unref: unsafe extern "C" fn(gpointer);
        g_free: unsafe extern "C" fn(gpointer);
        g_error_free: unsafe extern "C" fn(*mut GError);
    }
    gobject: ["libgobject-2.0.so.0"] {
        g_object_new: unsafe extern "C" fn(GType, *const c_char, ...) -> gpointer;
        g_object_set: unsafe extern "C" fn(gpointer, *const c_char, ...);
        g_object_ref: unsafe extern "C" fn(gpointer) -> gpointer;
        g_object_unref: unsafe extern "C" fn(gpointer);
        g_signal_connect_data: unsafe extern "C" fn(gpointer, *const c_char, GCallback, gpointer, gpointer, c_uint) -> u64;
    }
    gdk: ["libgdk-3.so.0"] {
        gdk_event_new: unsafe extern "C" fn(c_int) -> gpointer;
        gdk_event_free: unsafe extern "C" fn(gpointer);
        gdk_event_set_device: unsafe extern "C" fn(gpointer, gpointer);
        gdk_window_get_display: unsafe extern "C" fn(gpointer) -> gpointer;
        gdk_display_get_default_seat: unsafe extern "C" fn(gpointer) -> gpointer;
        gdk_seat_get_pointer: unsafe extern "C" fn(gpointer) -> gpointer;
        gdk_seat_get_keyboard: unsafe extern "C" fn(gpointer) -> gpointer;
    }
    gtk: ["libgtk-3.so.0"] {
        gtk_init_check: unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char) -> gboolean;
        gtk_offscreen_window_new: unsafe extern "C" fn() -> gpointer;
        gtk_window_set_default_size: unsafe extern "C" fn(gpointer, c_int, c_int);
        gtk_window_resize: unsafe extern "C" fn(gpointer, c_int, c_int);
        gtk_widget_set_size_request: unsafe extern "C" fn(gpointer, c_int, c_int);
        gtk_container_add: unsafe extern "C" fn(gpointer, gpointer);
        gtk_widget_show_all: unsafe extern "C" fn(gpointer);
        gtk_widget_get_window: unsafe extern "C" fn(gpointer) -> gpointer;
        gtk_widget_event: unsafe extern "C" fn(gpointer, gpointer) -> gboolean;
        gtk_widget_grab_focus: unsafe extern "C" fn(gpointer);
        gtk_widget_draw: unsafe extern "C" fn(gpointer, gpointer);
    }
    cairo: ["libcairo.so.2"] {
        cairo_image_surface_create: unsafe extern "C" fn(c_int, c_int, c_int) -> gpointer;
        cairo_create: unsafe extern "C" fn(gpointer) -> gpointer;
        cairo_destroy: unsafe extern "C" fn(gpointer);
        cairo_surface_destroy: unsafe extern "C" fn(gpointer);
        cairo_surface_flush: unsafe extern "C" fn(gpointer);
        cairo_image_surface_get_data: unsafe extern "C" fn(gpointer) -> *const u8;
        cairo_image_surface_get_width: unsafe extern "C" fn(gpointer) -> c_int;
        cairo_image_surface_get_height: unsafe extern "C" fn(gpointer) -> c_int;
        cairo_image_surface_get_stride: unsafe extern "C" fn(gpointer) -> c_int;
    }
    jsc: ["libjavascriptcoregtk-4.1.so.0"] {
        jsc_value_to_string: unsafe extern "C" fn(gpointer) -> *mut c_char;
    }
    webkit: ["libwebkit2gtk-4.1.so.0"] {
        webkit_get_major_version: unsafe extern "C" fn() -> c_uint;
        webkit_get_minor_version: unsafe extern "C" fn() -> c_uint;
        webkit_get_micro_version: unsafe extern "C" fn() -> c_uint;
        webkit_web_view_get_type: unsafe extern "C" fn() -> GType;
        webkit_web_context_new_ephemeral: unsafe extern "C" fn() -> gpointer;
        webkit_web_context_set_sandbox_enabled: unsafe extern "C" fn(gpointer, gboolean);
        webkit_web_context_set_cache_model: unsafe extern "C" fn(gpointer, c_int);
        webkit_web_context_get_website_data_manager: unsafe extern "C" fn(gpointer) -> gpointer;
        webkit_website_data_manager_set_network_proxy_settings: unsafe extern "C" fn(gpointer, c_int, gpointer);
        webkit_network_proxy_settings_new: unsafe extern "C" fn(*const c_char, *const *const c_char) -> gpointer;
        webkit_network_proxy_settings_free: unsafe extern "C" fn(gpointer);
        webkit_user_content_manager_new: unsafe extern "C" fn() -> gpointer;
        webkit_user_content_manager_add_filter: unsafe extern "C" fn(gpointer, gpointer);
        webkit_user_content_filter_store_new: unsafe extern "C" fn(*const c_char) -> gpointer;
        webkit_user_content_filter_store_save: unsafe extern "C" fn(gpointer, *const c_char, gpointer, gpointer, GCallback, gpointer);
        webkit_user_content_filter_store_save_finish: unsafe extern "C" fn(gpointer, gpointer, *mut *mut GError) -> gpointer;
        webkit_settings_new: unsafe extern "C" fn() -> gpointer;
        webkit_web_view_load_html: unsafe extern "C" fn(gpointer, *const c_char, *const c_char);
        webkit_web_view_set_zoom_level: unsafe extern "C" fn(gpointer, f64);
        webkit_web_view_get_title: unsafe extern "C" fn(gpointer) -> *const c_char;
        webkit_web_view_get_uri: unsafe extern "C" fn(gpointer) -> *const c_char;
        webkit_web_view_evaluate_javascript: unsafe extern "C" fn(gpointer, *const c_char, isize, *const c_char, *const c_char, gpointer, GCallback, gpointer);
        webkit_web_view_evaluate_javascript_finish: unsafe extern "C" fn(gpointer, gpointer, *mut *mut GError) -> gpointer;
        webkit_navigation_policy_decision_get_navigation_action: unsafe extern "C" fn(gpointer) -> gpointer;
        webkit_navigation_action_get_request: unsafe extern "C" fn(gpointer) -> gpointer;
        webkit_navigation_action_get_navigation_type: unsafe extern "C" fn(gpointer) -> c_int;
        webkit_navigation_action_is_user_gesture: unsafe extern "C" fn(gpointer) -> gboolean;
        webkit_uri_request_get_uri: unsafe extern "C" fn(gpointer) -> *const c_char;
        webkit_policy_decision_use: unsafe extern "C" fn(gpointer);
        webkit_policy_decision_ignore: unsafe extern "C" fn(gpointer);
        webkit_permission_request_deny: unsafe extern "C" fn(gpointer);
        webkit_download_cancel: unsafe extern "C" fn(gpointer);
    }
});

/// `GdkEventType` values used here.
pub const GDK_MOTION_NOTIFY: c_int = 3;
pub const GDK_BUTTON_PRESS: c_int = 4;
pub const GDK_BUTTON_RELEASE: c_int = 7;
pub const GDK_KEY_PRESS: c_int = 8;
pub const GDK_KEY_RELEASE: c_int = 9;
pub const GDK_SCROLL: c_int = 31;
/// `GDK_SCROLL_SMOOTH`.
pub const GDK_SCROLL_SMOOTH: c_int = 4;
/// `GdkModifierType` bits.
pub const GDK_SHIFT_MASK: u32 = 1;
pub const GDK_CONTROL_MASK: u32 = 1 << 2;
pub const GDK_BUTTON1_MASK: u32 = 1 << 8;

/// `GdkEventMotion` (GTK 3).
#[repr(C)]
pub struct EventMotion {
    pub kind: c_int,
    pub window: gpointer,
    pub send_event: i8,
    pub time: u32,
    pub x: f64,
    pub y: f64,
    pub axes: *mut f64,
    pub state: u32,
    pub is_hint: i16,
    pub device: gpointer,
    pub x_root: f64,
    pub y_root: f64,
}

/// `GdkEventButton` (GTK 3).
#[repr(C)]
pub struct EventButton {
    pub kind: c_int,
    pub window: gpointer,
    pub send_event: i8,
    pub time: u32,
    pub x: f64,
    pub y: f64,
    pub axes: *mut f64,
    pub state: u32,
    pub button: u32,
    pub device: gpointer,
    pub x_root: f64,
    pub y_root: f64,
}

/// `GdkEventScroll` (GTK 3; `is_stop` is a one-bit field in a `guint`).
#[repr(C)]
pub struct EventScroll {
    pub kind: c_int,
    pub window: gpointer,
    pub send_event: i8,
    pub time: u32,
    pub x: f64,
    pub y: f64,
    pub state: u32,
    pub direction: c_int,
    pub device: gpointer,
    pub x_root: f64,
    pub y_root: f64,
    pub delta_x: f64,
    pub delta_y: f64,
    pub is_stop: u32,
}

/// `GdkEventKey` (GTK 3; `is_modifier` is a one-bit field in a `guint`).
#[repr(C)]
pub struct EventKey {
    pub kind: c_int,
    pub window: gpointer,
    pub send_event: i8,
    pub time: u32,
    pub state: u32,
    pub keyval: u32,
    pub length: c_int,
    pub string: *mut c_char,
    pub hardware_keycode: u16,
    pub group: u8,
    pub is_modifier: u32,
}

/// WebKit enum values used here.
pub const WEBKIT_LOAD_FINISHED: c_int = 3;
pub const WEBKIT_POLICY_DECISION_TYPE_RESPONSE: c_int = 2;
pub const WEBKIT_NAVIGATION_TYPE_LINK_CLICKED: c_int = 0;
pub const WEBKIT_CACHE_MODEL_DOCUMENT_VIEWER: c_int = 0;
pub const WEBKIT_NETWORK_PROXY_MODE_CUSTOM: c_int = 2;
pub const WEBKIT_HARDWARE_ACCELERATION_POLICY_NEVER: c_int = 2;
