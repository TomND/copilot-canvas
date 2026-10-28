//! The libghostty-vt engine behind [`Emulator`] (feature `ghostty`).
//!
//! A [`GhosttyEmulator`] owns a Ghostty terminal, a render state and a key
//! encoder. Ghostty reports side effects (replies, title, bell, OSC 52,
//! progress, notifications, synchronized output) through C callbacks while
//! `vt_write` runs; the callbacks only record them in [`Shared`] and `feed`
//! delivers them afterwards, so nothing re-enters the terminal.
//!
//! Snapshots are read from the render state. While a program holds the
//! screen with synchronized output (DEC 2026) the render state captured when
//! the hold began is shown, as alacritty shows the screen from before its
//! buffered sync, until the hold ends or [`SYNC_TIMEOUT`] passes. The display
//! offset, history size and modes of a held snapshot are captured together
//! with that render state ([`View`]); hyperlinks can only be looked up in the
//! live terminal, so a held snapshot carries none (they return with the next
//! frame after the hold, at most [`SYNC_TIMEOUT`] later).
//!
//! Key input goes through Ghostty's key encoder (kitty keyboard protocol
//! included). In legacy mode the few xterm extensions that ConPTY's input
//! parser does not map to keys are replaced with the `crate::keys` bytes (see
//! [`is_legacy_extension`]). Mouse reports follow `crate::keys` exactly.
//!
//! OSC 52 writes reach the clipboard only for the standard clipboard and up
//! to [`CLIPBOARD_MAX_BYTES`].

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void};
use std::time::{Duration, Instant};

use alacritty_terminal::vte::ansi::NamedColor;
use canvas_common::protocol::*;
use ghostty_vt_sys as sys;

use super::alacritty::{CLIPBOARD_MAX_BYTES, palette};
use super::{Config, Emulator, Engine, Events, Scroll, Snapshot};

/// How long a synchronized update may hold the screen (alacritty's limit).
pub const SYNC_TIMEOUT: Duration = Duration::from_millis(150);
/// Cell size reported to size queries (XTWINOPS), as alacritty reports it.
const CELL_WIDTH: u32 = 8;
const CELL_HEIGHT: u32 = 16;

/// An event recorded by a callback during `vt_write`.
enum Pending {
    Reply(Vec<u8>),
    Bell,
    Clipboard(String),
    Progress(u8),
    Notify,
}

/// State the callbacks write to. Reached only through the raw pointer the
/// terminal holds as userdata (and the emulator's copy), never through a
/// reference that outlives one call, so callbacks never alias a live borrow.
struct Shared {
    pending: Vec<Pending>,
    /// OSC 0/2 or a full reset changed the title; read once after the write.
    title_changed: bool,
    cols: u16,
    rows: u16,
    /// A synchronized update began at this time and has not ended.
    hold: Option<Instant>,
    /// Captured when a hold begins (see `on_render_hold`).
    render: sys::GhosttyRenderState,
    /// What a held snapshot shows besides the cells, read with `render`.
    held: View,
}

/// The parts of a snapshot read from the terminal rather than the render
/// state; captured together with the render state during a hold.
#[derive(Clone, Copy, Default)]
struct View {
    display_offset: u32,
    history: u32,
    modes: u32,
}

impl View {
    /// The live values (`terminal` must be valid).
    fn read(terminal: sys::GhosttyTerminal) -> Self {
        // SAFETY: SCROLLBACK_ROWS yields a usize.
        let history = unsafe { terminal_get(terminal, sys::GHOSTTY_TERMINAL_DATA_SCROLLBACK_ROWS, 0usize) }.unwrap_or(0);
        Self { display_offset: display_offset(terminal), history: u32::try_from(history).unwrap_or(u32::MAX), modes: modes(terminal) }
    }
}

/// Hyperlink targets of one snapshot, deduplicated.
#[derive(Default)]
struct Links {
    targets: Vec<String>,
    /// Target -> 1-based id (`Run8::link`).
    ids: HashMap<String, u32>,
    /// The URI read for the current cell.
    uri: Vec<u8>,
    /// The previous linked cell's URI and id: runs of linked cells reuse it
    /// without hashing or allocating (the C API has no per-cell link id).
    previous: Vec<u8>,
    previous_id: u32,
}

impl Links {
    /// The id of the URI in `self.uri`.
    fn intern(&mut self) -> u32 {
        if self.previous_id != 0 && self.uri == self.previous {
            return self.previous_id;
        }
        let uri = String::from_utf8_lossy(&self.uri);
        let id = match self.ids.get(uri.as_ref()) {
            Some(&id) => id,
            None => {
                let uri = uri.into_owned();
                self.targets.push(uri.clone());
                let id = self.targets.len() as u32;
                self.ids.insert(uri, id);
                id
            }
        };
        std::mem::swap(&mut self.uri, &mut self.previous);
        self.previous_id = id;
        id
    }
}

/// The Ghostty engine. `!Send`: it lives on its terminal's emulator thread.
/// Ghostty always answers kitty keyboard queries, so `Config::kitty_keyboard`
/// has no effect here.
pub struct GhosttyEmulator {
    terminal: sys::GhosttyTerminal,
    render: sys::GhosttyRenderState,
    rows: sys::GhosttyRenderStateRowIterator,
    cells: sys::GhosttyRenderStateRowCells,
    encoder: sys::GhosttyKeyEncoder,
    key_event: sys::GhosttyKeyEvent,
    shared: *mut Shared,
}

fn check(result: sys::GhosttyResult, what: &str) -> anyhow::Result<()> {
    anyhow::ensure!(result == sys::GHOSTTY_SUCCESS, "{what} failed (result {result})");
    Ok(())
}

impl GhosttyEmulator {
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        let cols = config.cols.max(1);
        let rows = config.rows.max(1);
        let shared = Box::into_raw(Box::new(Shared {
            pending: Vec::new(),
            title_changed: false,
            cols,
            rows,
            hold: None,
            render: std::ptr::null_mut(),
            held: View::default(),
        }));
        // Drop frees whatever was created if a later step fails.
        let mut emu = Self {
            terminal: std::ptr::null_mut(),
            render: std::ptr::null_mut(),
            rows: std::ptr::null_mut(),
            cells: std::ptr::null_mut(),
            encoder: std::ptr::null_mut(),
            key_event: std::ptr::null_mut(),
            shared,
        };
        let null = std::ptr::null();
        // SAFETY: each call writes one handle owned by `emu`.
        unsafe {
            check(sys::ghostty_terminal_new(null, &mut emu.terminal, cols, rows), "ghostty_terminal_new")?;
            check(sys::ghostty_render_state_new(null, &mut emu.render), "ghostty_render_state_new")?;
            check(sys::ghostty_render_state_row_iterator_new(null, &mut emu.rows), "ghostty_render_state_row_iterator_new")?;
            check(sys::ghostty_render_state_row_cells_new(null, &mut emu.cells), "ghostty_render_state_row_cells_new")?;
            check(sys::ghostty_key_encoder_new(null, &mut emu.encoder), "ghostty_key_encoder_new")?;
            check(sys::ghostty_key_event_new(null, &mut emu.key_event), "ghostty_key_event_new")?;
            (*emu.shared).render = emu.render;
        }
        emu.configure(config.scrollback)?;
        Ok(emu)
    }

    fn configure(&mut self, scrollback: usize) -> anyhow::Result<()> {
        let t = self.terminal;
        let (cols, rows) = (self.shared().cols, self.shared().rows);
        let rgb = |c: alacritty_terminal::vte::ansi::Rgb| sys::GhosttyColorRgb { r: c.r, g: c.g, b: c.b };
        // The same colours as the alacritty engine, for OSC 4/10/11 queries.
        let colors: [sys::GhosttyColorRgb; 256] = std::array::from_fn(|i| rgb(palette(i)));
        let foreground = rgb(palette(NamedColor::Foreground as usize));
        let background = rgb(palette(NamedColor::Background as usize));
        let terminfo = "xterm-256color";
        let terminfo = sys::GhosttyString { ptr: terminfo.as_ptr(), len: terminfo.len() };
        let pull_scrollback = false;
        macro_rules! set {
            ($option:ident, $value:expr) => {
                // SAFETY: the value has the type the option documents; values
                // are copied by the terminal, callbacks and userdata outlive it.
                check(unsafe { sys::ghostty_terminal_set(t, sys::$option, $value) }, stringify!($option))?
            };
        }
        macro_rules! callback {
            ($option:ident, $ty:ident, $f:expr) => {{
                let f: sys::$ty = Some($f);
                set!($option, f.map_or(std::ptr::null(), |f| f as *const c_void));
            }};
        }
        // SAFETY: the terminal handle is valid.
        check(unsafe { sys::ghostty_terminal_resize(t, cols, rows, CELL_WIDTH, CELL_HEIGHT) }, "ghostty_terminal_resize")?;
        set!(GHOSTTY_TERMINAL_OPT_USERDATA, self.shared.cast_const().cast());
        callback!(GHOSTTY_TERMINAL_OPT_WRITE_PTY, GhosttyTerminalWritePtyFn, on_write_pty);
        callback!(GHOSTTY_TERMINAL_OPT_BELL, GhosttyTerminalBellFn, on_bell);
        callback!(GHOSTTY_TERMINAL_OPT_TITLE_CHANGED, GhosttyTerminalTitleChangedFn, on_title_changed);
        callback!(GHOSTTY_TERMINAL_OPT_RESET, GhosttyTerminalResetFn, on_title_changed);
        callback!(GHOSTTY_TERMINAL_OPT_SIZE, GhosttyTerminalSizeFn, on_size);
        callback!(GHOSTTY_TERMINAL_OPT_DEVICE_ATTRIBUTES, GhosttyTerminalDeviceAttributesFn, on_device_attributes);
        callback!(GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE, GhosttyTerminalClipboardWriteFn, on_clipboard_write);
        callback!(GHOSTTY_TERMINAL_OPT_DESKTOP_NOTIFICATION, GhosttyTerminalDesktopNotificationFn, on_notification);
        callback!(GHOSTTY_TERMINAL_OPT_PROGRESS_REPORT, GhosttyTerminalProgressReportFn, on_progress);
        callback!(GHOSTTY_TERMINAL_OPT_RENDER_HOLD, GhosttyTerminalRenderHoldFn, on_render_hold);
        // No GHOSTTY_TERMINAL_OPT_CLIPBOARD_READ: OSC 52 reads stay unanswered.
        set!(GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES, (&raw const scrollback).cast());
        set!(GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND, (&raw const foreground).cast());
        set!(GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND, (&raw const background).cast());
        set!(GHOSTTY_TERMINAL_OPT_COLOR_PALETTE, colors.as_ptr().cast());
        set!(GHOSTTY_TERMINAL_OPT_TERMINFO_NAME, (&raw const terminfo).cast());
        // ConPTY keeps no scrollback of its own: a resize must not pull rows back.
        set!(GHOSTTY_TERMINAL_OPT_RESIZE_PULL_SCROLLBACK, (&raw const pull_scrollback).cast());
        // Bounds OSC 5522 transactions; OSC 52 writes are checked in `on_clipboard_write`.
        let clipboard_max: usize = CLIPBOARD_MAX_BYTES;
        set!(GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE_MAX_BYTES, (&raw const clipboard_max).cast());
        Ok(())
    }

    fn shared(&self) -> &Shared {
        // SAFETY: allocated in `new`, freed in `drop`; callbacks only run
        // inside FFI calls, while no reference from here is alive.
        unsafe { &*self.shared }
    }

    fn shared_mut(&mut self) -> &mut Shared {
        // SAFETY: as in `shared`.
        unsafe { &mut *self.shared }
    }

    /// Reads one `GHOSTTY_TERMINAL_DATA_*` value.
    ///
    /// # Safety
    /// `T` must be the output type the data kind documents.
    unsafe fn get<T>(&self, data: sys::GhosttyTerminalData, value: T) -> Option<T> {
        // SAFETY: upheld by the caller.
        unsafe { terminal_get(self.terminal, data, value) }
    }

    fn mode(&self, mode: sys::GhosttyMode) -> bool {
        // SAFETY: DATA_MODE takes a GhosttyTerminalModeConfig.
        unsafe { self.get(sys::GHOSTTY_TERMINAL_DATA_MODE, sys::GhosttyTerminalModeConfig { mode, value: false }) }.is_some_and(|m| m.value)
    }

    fn title(&self) -> Option<String> {
        let empty = sys::GhosttyString { ptr: std::ptr::null(), len: 0 };
        // SAFETY: DATA_TITLE yields a GhosttyString borrowed until the next mutation.
        let title = unsafe { self.get(sys::GHOSTTY_TERMINAL_DATA_TITLE, empty) }?;
        // SAFETY: valid now; copied before any other call.
        let bytes = unsafe { title.as_bytes() };
        (!bytes.is_empty()).then(|| String::from_utf8_lossy(bytes).into_owned())
    }

    fn drain(&mut self, events: &mut dyn Events) {
        let shared = self.shared_mut();
        let pending = std::mem::take(&mut shared.pending);
        let title_changed = std::mem::take(&mut shared.title_changed);
        for item in pending {
            match item {
                Pending::Reply(bytes) => events.reply(bytes),
                Pending::Bell => events.bell(),
                Pending::Clipboard(text) => events.clipboard(text),
                Pending::Progress(state) => events.progress(state),
                Pending::Notify => events.notify(),
            }
        }
        if title_changed {
            events.title(self.title());
        }
    }

    /// One viewport row as protocol-8 runs (same merging and trimming as
    /// alacritty). Hyperlinks are looked up only when `links` is given.
    ///
    /// # Safety
    /// `rows` must be positioned on viewport row `y` of the current render state.
    unsafe fn row_runs(&self, rows: sys::GhosttyRenderStateRowIterator, y: u16, mut links: Option<&mut Links>) -> Vec<Run8> {
        let mut view = sys::GhosttyCellsView { ptr: std::ptr::null(), len: 0 };
        // SAFETY: CELLS_RAW yields a view valid until the next render-state update.
        if unsafe { sys::ghostty_render_state_row_get(rows, sys::GHOSTTY_RENDER_STATE_ROW_DATA_CELLS_RAW, (&raw mut view).cast()) }
            != sys::GHOSTTY_SUCCESS
            || view.ptr.is_null()
        {
            return Vec::new();
        }
        // SAFETY: `len` cells at `ptr`, see above.
        let raw = unsafe { std::slice::from_raw_parts(view.ptr, view.len) };
        // An all-zero cell is an empty default cell, which trimming removes anyway.
        let end = raw.iter().rposition(|&c| c != 0).map_or(0, |i| i + 1);
        if end == 0 {
            return Vec::new();
        }
        let mut cells = self.cells;
        // SAFETY: fills the row-cells handle (pointed to) for this row.
        let have_cells = unsafe { sys::ghostty_render_state_row_get(rows, sys::GHOSTTY_RENDER_STATE_ROW_DATA_CELLS, (&raw mut cells).cast()) }
            == sys::GHOSTTY_SUCCESS;
        let mut runs: Vec<Run8> = Vec::new();
        let mut style_cache: Option<(u16, CellStyle)> = None;
        for (x, &raw_cell) in raw[..end].iter().enumerate() {
            let x = x as u16;
            let cell = CellInfo::decode(raw_cell);
            if cell.wide == sys::GHOSTTY_CELL_WIDE_SPACER_TAIL || cell.wide == sys::GHOSTTY_CELL_WIDE_SPACER_HEAD {
                continue;
            }
            let mut style = CellStyle::default();
            if cell.styled && have_cells {
                style = match style_cache {
                    Some((id, style)) if id == cell.style_id => style,
                    _ => {
                        // SAFETY: `cells` holds this row; x < row length.
                        let fetched = unsafe { cell_style(cells, x) };
                        style_cache = Some((cell.style_id, fetched));
                        fetched
                    }
                };
            }
            match cell.tag {
                sys::GHOSTTY_CELL_CONTENT_BG_COLOR_PALETTE => {
                    let mut index: u8 = 0;
                    // SAFETY: COLOR_PALETTE writes a GhosttyColorPaletteIndex.
                    if unsafe { sys::ghostty_cell_get(raw_cell, sys::GHOSTTY_CELL_DATA_COLOR_PALETTE, (&raw mut index).cast()) } == sys::GHOSTTY_SUCCESS {
                        style.bg = index as u32 + 1;
                    }
                }
                sys::GHOSTTY_CELL_CONTENT_BG_COLOR_RGB => {
                    let mut rgb = sys::GhosttyColorRgb::default();
                    // SAFETY: COLOR_RGB writes a GhosttyColorRgb.
                    if unsafe { sys::ghostty_cell_get(raw_cell, sys::GHOSTTY_CELL_DATA_COLOR_RGB, (&raw mut rgb).cast()) } == sys::GHOSTTY_SUCCESS {
                        style.bg = rgb_color(rgb);
                    }
                }
                _ => {}
            }
            let first = char::from_u32(cell.codepoint).filter(|&c| c != '\0' && c != '\t').unwrap_or(' ');
            let extra = if cell.tag == sys::GHOSTTY_CELL_CONTENT_CODEPOINT_GRAPHEME && have_cells {
                // SAFETY: as for `cell_style`.
                unsafe { grapheme_extra(cells, x) }
            } else {
                Vec::new()
            };
            let link = match links.as_deref_mut() {
                Some(links) if cell.link => {
                    if self.hyperlink(x, y, &mut links.uri) { links.intern() } else { 0 }
                }
                _ => 0,
            };
            let wide = cell.wide == sys::GHOSTTY_CELL_WIDE_WIDE;
            let same = |run: &Run8| {
                run.fg == style.fg
                    && run.bg == style.bg
                    && run.flags == style.flags
                    && run.link == link
                    && run.wide == wide
                    && run.underline == style.underline
            };
            let run = match runs.last_mut() {
                Some(run) if same(run) => run,
                _ => {
                    runs.push(Run8 {
                        col: x,
                        fg: style.fg,
                        bg: style.bg,
                        flags: style.flags,
                        link,
                        wide,
                        underline: style.underline,
                        ..Default::default()
                    });
                    runs.last_mut().expect("just pushed")
                }
            };
            if !extra.is_empty() {
                let index = run.text.chars().count() - run.clusters.iter().map(|(_, n)| *n as usize).sum::<usize>();
                run.clusters.push((index as u16, extra.len().min(255) as u8));
            }
            run.text.push(first);
            run.text.extend(extra.iter().take(255));
        }
        trim_trailing_blank(&mut runs);
        runs
    }

    /// Reads the OSC 8 target of a live viewport cell into `uri`; `false` when it has none.
    fn hyperlink(&self, x: u16, y: u16, uri: &mut Vec<u8>) -> bool {
        let mut value = sys::GhosttyPointValue { _padding: [0; 2] };
        value.coordinate = sys::GhosttyPointCoordinate { x, y: y as u32 };
        let point = sys::GhosttyPoint { tag: sys::GHOSTTY_POINT_TAG_VIEWPORT, value };
        let mut grid_ref = sys::GhosttyGridRef::sized();
        // SAFETY: the terminal handle is valid; the ref is used before any mutation.
        if unsafe { sys::ghostty_terminal_grid_ref(self.terminal, point, &mut grid_ref) } != sys::GHOSTTY_SUCCESS {
            return false;
        }
        // Reuses the buffer's allocation from cell to cell.
        let capacity = uri.capacity().max(256);
        uri.clear();
        uri.resize(capacity, 0);
        let mut len = 0usize;
        // SAFETY: `uri` has `uri.len()` writable bytes.
        let mut result = unsafe { sys::ghostty_grid_ref_hyperlink_uri(&grid_ref, uri.as_mut_ptr(), uri.len(), &mut len) };
        if result == sys::GHOSTTY_OUT_OF_SPACE && len > uri.len() {
            uri.resize(len, 0);
            // SAFETY: as above, with the required size.
            result = unsafe { sys::ghostty_grid_ref_hyperlink_uri(&grid_ref, uri.as_mut_ptr(), uri.len(), &mut len) };
        }
        let found = result == sys::GHOSTTY_SUCCESS && len > 0 && len <= uri.len();
        uri.truncate(if found { len } else { 0 });
        found
    }

    fn encode_paste(&self, text: &str) -> Vec<u8> {
        let bracketed = self.mode(sys::GHOSTTY_MODE_BRACKETED_PASTE);
        // A pasted bracket marker would let the text end the paste and inject keystrokes.
        let mut clean = text.replace("\x1b[201~", "").replace("\x1b[200~", "");
        if !bracketed {
            clean = clean.replace("\r\n", "\r");
        }
        let mut data = clean.into_bytes();
        let mut out = vec![0u8; data.len() + 16];
        let mut written = 0usize;
        // SAFETY: `data` is rewritten in place (same length); `out` has `out.len()` bytes.
        let mut result = unsafe {
            sys::ghostty_paste_encode(data.as_mut_ptr().cast(), data.len(), bracketed, out.as_mut_ptr().cast(), out.len(), &mut written)
        };
        if result == sys::GHOSTTY_OUT_OF_SPACE {
            out.resize(written, 0);
            // SAFETY: as above; re-encoding already cleaned data is idempotent.
            result = unsafe {
                sys::ghostty_paste_encode(data.as_mut_ptr().cast(), data.len(), bracketed, out.as_mut_ptr().cast(), out.len(), &mut written)
            };
        }
        if result != sys::GHOSTTY_SUCCESS {
            return Vec::new();
        }
        out.truncate(written);
        out
    }

    fn encode_focus(&self, focused: bool) -> Vec<u8> {
        if !self.mode(sys::GHOSTTY_MODE_FOCUS_EVENT) {
            return Vec::new();
        }
        let event = if focused { sys::GHOSTTY_FOCUS_GAINED } else { sys::GHOSTTY_FOCUS_LOST };
        let mut buf = [0u8; 8];
        let mut written = 0usize;
        // SAFETY: `buf` has `buf.len()` writable bytes.
        let result = unsafe { sys::ghostty_focus_encode(event, buf.as_mut_ptr().cast(), buf.len(), &mut written) };
        if result == sys::GHOSTTY_SUCCESS { buf[..written.min(buf.len())].to_vec() } else { Vec::new() }
    }

    /// Ghostty's encoding of a key, configured from the terminal's modes.
    fn ghostty_key(&mut self, spec: &KeySpec) -> Vec<u8> {
        let mut text = [0u8; 4];
        let text = spec.text.map(|c| c.encode_utf8(&mut text).as_bytes()).unwrap_or_default();
        let event = self.key_event;
        // SAFETY: encoder, event and terminal handles are valid; `text` outlives the encode.
        unsafe {
            sys::ghostty_key_encoder_setopt_from_terminal(self.encoder, self.terminal);
            sys::ghostty_key_event_set_action(event, sys::GHOSTTY_KEY_ACTION_PRESS);
            sys::ghostty_key_event_set_key(event, spec.key);
            sys::ghostty_key_event_set_mods(event, spec.mods);
            // A character key's text already reflects Shift.
            let consumed = if spec.text.is_some() { spec.mods & sys::GHOSTTY_MODS_SHIFT } else { 0 };
            sys::ghostty_key_event_set_consumed_mods(event, consumed);
            sys::ghostty_key_event_set_composing(event, false);
            sys::ghostty_key_event_set_utf8(event, if text.is_empty() { std::ptr::null() } else { text.as_ptr().cast::<c_char>() }, text.len());
            sys::ghostty_key_event_set_unshifted_codepoint(event, spec.unshifted);
            let mut out = vec![0u8; 64];
            let mut len = 0usize;
            let mut result = sys::ghostty_key_encoder_encode(self.encoder, event, out.as_mut_ptr().cast(), out.len(), &mut len);
            if result == sys::GHOSTTY_OUT_OF_SPACE {
                out.resize(len, 0);
                result = sys::ghostty_key_encoder_encode(self.encoder, event, out.as_mut_ptr().cast(), out.len(), &mut len);
            }
            // Do not keep a pointer to the stack buffer in the event.
            sys::ghostty_key_event_set_utf8(event, std::ptr::null(), 0);
            if result != sys::GHOSTTY_SUCCESS {
                return Vec::new();
            }
            out.truncate(len);
            out
        }
    }

    fn encode_key(&mut self, key: Key, mods: u8) -> Vec<u8> {
        let modes = self.modes();
        let legacy = || crate::keys::encode(&Input::Key { key, mods }, modes);
        let Some(spec) = KeySpec::new(key, mods) else { return legacy() };
        let bytes = self.ghostty_key(&spec);
        let kitty = modes >> MODE_KITTY_SHIFT & 0x1f != 0;
        if bytes.is_empty() || (!kitty && is_legacy_extension(&bytes)) { legacy() } else { bytes }
    }
}

impl Drop for GhosttyEmulator {
    fn drop(&mut self) {
        // SAFETY: every handle was created by `new` (or is NULL, which the
        // free functions accept) and is freed exactly once; the terminal goes
        // before the state its callbacks point to.
        unsafe {
            sys::ghostty_key_event_free(self.key_event);
            sys::ghostty_key_encoder_free(self.encoder);
            sys::ghostty_render_state_row_cells_free(self.cells);
            sys::ghostty_render_state_row_iterator_free(self.rows);
            sys::ghostty_terminal_free(self.terminal);
            sys::ghostty_render_state_free(self.render);
            drop(Box::from_raw(self.shared));
        }
    }
}

impl Emulator for GhosttyEmulator {
    fn engine(&self) -> Engine {
        Engine::Ghostty
    }

    fn feed(&mut self, bytes: &[u8], events: &mut dyn Events) {
        if !bytes.is_empty() {
            // SAFETY: the callbacks only touch `Shared` and never write to the terminal.
            unsafe { sys::ghostty_terminal_vt_write(self.terminal, bytes.as_ptr(), bytes.len()) };
        }
        self.drain(events);
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let shared = self.shared_mut();
        (shared.cols, shared.rows) = (cols, rows);
        // SAFETY: valid handle; ends any hold through `on_render_hold`.
        unsafe { sys::ghostty_terminal_resize(self.terminal, cols, rows, CELL_WIDTH, CELL_HEIGHT) };
    }

    fn snapshot(&self) -> Snapshot {
        // Copied out: no reference to `Shared` may live across FFI calls.
        let (held, shared_cols, shared_rows, held_view) = {
            let shared = self.shared();
            (shared.hold.is_some(), shared.cols, shared.rows, shared.held)
        };
        let blank = || Snapshot { cols: shared_cols, rows: shared_rows, lines: vec![Vec::new(); shared_rows as usize], modes: self.modes(), ..Snapshot::default() };
        // SAFETY: all handles are valid and used on this thread only; row and
        // cell data are read before the render state is updated again.
        unsafe {
            if !held && sys::ghostty_render_state_update(self.render, self.terminal) != sys::GHOSTTY_SUCCESS {
                return blank();
            }
            let (mut cols, mut rows) = (0u16, 0u16);
            if sys::ghostty_render_state_get(self.render, sys::GHOSTTY_RENDER_STATE_DATA_COLS, (&raw mut cols).cast()) != sys::GHOSTTY_SUCCESS
                || sys::ghostty_render_state_get(self.render, sys::GHOSTTY_RENDER_STATE_DATA_ROWS, (&raw mut rows).cast()) != sys::GHOSTTY_SUCCESS
            {
                return blank();
            }
            // A held frame takes everything from the moment it was captured;
            // links exist only in the live terminal (see the module docs).
            let view = if held { held_view } else { View::read(self.terminal) };
            let mut links = (!held).then(Links::default);
            let mut lines = Vec::with_capacity(rows as usize);
            let mut iterator = self.rows;
            if sys::ghostty_render_state_get(self.render, sys::GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR, (&raw mut iterator).cast())
                == sys::GHOSTTY_SUCCESS
            {
                while lines.len() < rows as usize && sys::ghostty_render_state_row_iterator_next(iterator) {
                    let y = lines.len() as u16;
                    lines.push(self.row_runs(iterator, y, links.as_mut()));
                }
            }
            lines.resize(rows as usize, Vec::new());
            let mut cursor = sys::GhosttyRenderStateCursor::sized();
            let have_cursor =
                sys::ghostty_render_state_get(self.render, sys::GHOSTTY_RENDER_STATE_DATA_CURSOR, (&raw mut cursor).cast()) == sys::GHOSTTY_SUCCESS;
            let cursor_shape = match cursor.visual_style {
                _ if !have_cursor => CURSOR_BLOCK,
                sys::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BAR => CURSOR_BAR,
                sys::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE => CURSOR_UNDERLINE,
                sys::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW => CURSOR_BLOCK_HOLLOW,
                _ => CURSOR_BLOCK,
            };
            Snapshot {
                cols,
                rows,
                lines,
                links: links.map(|links| links.targets).unwrap_or_default(),
                cursor: (have_cursor && view.display_offset == 0 && cursor.visible && cursor.viewport_has_value)
                    .then_some((cursor.viewport_x, cursor.viewport_y)),
                cursor_shape,
                cursor_blink: have_cursor && cursor.blinking,
                display_offset: view.display_offset,
                history: view.history,
                modes: view.modes,
            }
        }
    }

    fn scroll(&mut self, scroll: Scroll) {
        let mut behavior = sys::GhosttyTerminalScrollViewport {
            tag: sys::GHOSTTY_SCROLL_VIEWPORT_BOTTOM,
            value: sys::GhosttyTerminalScrollViewportValue { _padding: [0; 2] },
        };
        match scroll {
            // Ghostty scrolls up (towards history) with negative deltas.
            Scroll::Delta(delta) => {
                behavior.tag = sys::GHOSTTY_SCROLL_VIEWPORT_DELTA;
                behavior.value.delta = -(delta as isize);
            }
            Scroll::Top => behavior.tag = sys::GHOSTTY_SCROLL_VIEWPORT_TOP,
            Scroll::Bottom => {}
        }
        // SAFETY: valid handle.
        unsafe { sys::ghostty_terminal_scroll_viewport(self.terminal, behavior) };
        // Scrolling shows live content even during a hold.
        // SAFETY: valid handles.
        if self.shared().hold.is_some() && unsafe { sys::ghostty_render_state_update(self.render, self.terminal) } == sys::GHOSTTY_SUCCESS {
            let view = View::read(self.terminal);
            self.shared_mut().held = view;
        }
    }

    fn display_offset(&self) -> u32 {
        display_offset(self.terminal)
    }

    fn modes(&self) -> u32 {
        modes(self.terminal)
    }

    fn encode(&mut self, input: &Input) -> Vec<u8> {
        match input {
            Input::Raw(bytes) => bytes.clone(),
            Input::Paste(text) => self.encode_paste(text),
            Input::Focus(focused) => self.encode_focus(*focused),
            Input::Key { key, mods } => self.encode_key(*key, *mods),
        }
    }

    fn encode_wheel(&mut self, up: bool, col: u16, row: u16, mods: u8) -> Option<Vec<u8>> {
        // The wheel policy (Shift scrolls history, PageUp/PageDown on the
        // alternate screen) is the host's, so the bytes come from `crate::keys`.
        crate::keys::encode_wheel(up, col, row, mods, self.modes())
    }

    fn encode_click(&mut self, button: u8, col: u16, row: u16, mods: u8) -> Option<Vec<u8>> {
        crate::keys::encode_click(button, col, row, mods, self.modes())
    }

    fn sync_deadline(&self) -> Option<Instant> {
        self.shared().hold.map(|started| started + SYNC_TIMEOUT)
    }

    fn end_sync(&mut self, events: &mut dyn Events) {
        let mode = sys::GhosttyTerminalModeConfig { mode: sys::GHOSTTY_MODE_SYNC_OUTPUT, value: false };
        // SAFETY: OPT_MODE takes a GhosttyTerminalModeConfig; it does not call `on_render_hold`.
        unsafe { sys::ghostty_terminal_set(self.terminal, sys::GHOSTTY_TERMINAL_OPT_MODE, (&raw const mode).cast()) };
        self.shared_mut().hold = None;
        self.drain(events);
    }
}

/// Reads one `GHOSTTY_TERMINAL_DATA_*` value.
///
/// # Safety
/// `terminal` must be valid and `T` the output type the data kind documents.
unsafe fn terminal_get<T>(terminal: sys::GhosttyTerminal, data: sys::GhosttyTerminalData, mut value: T) -> Option<T> {
    // SAFETY: upheld by the caller.
    let result = unsafe { sys::ghostty_terminal_get(terminal, data, (&raw mut value).cast()) };
    (result == sys::GHOSTTY_SUCCESS).then_some(value)
}

/// Lines between the viewport and the live screen (`terminal` must be valid).
fn display_offset(terminal: sys::GhosttyTerminal) -> u32 {
    // SAFETY: DATA_SCROLLBAR yields a GhosttyTerminalScrollbar.
    let bar = unsafe { terminal_get(terminal, sys::GHOSTTY_TERMINAL_DATA_SCROLLBAR, sys::GhosttyTerminalScrollbar::default()) }.unwrap_or_default();
    u32::try_from(bar.total.saturating_sub(bar.offset).saturating_sub(bar.len)).unwrap_or(u32::MAX)
}

/// `MODE_*` bits: one `get_multi` call, or one query per value if it fails
/// (`terminal` must be valid).
fn modes(terminal: sys::GhosttyTerminal) -> u32 {
    const MODES: [(sys::GhosttyMode, u32); 11] = [
        (sys::GHOSTTY_MODE_X10_MOUSE, MODE_MOUSE),
        (sys::GHOSTTY_MODE_NORMAL_MOUSE, MODE_MOUSE),
        (sys::GHOSTTY_MODE_BUTTON_MOUSE, MODE_MOUSE | MODE_MOUSE_DRAG),
        (sys::GHOSTTY_MODE_ANY_MOUSE, MODE_MOUSE | MODE_MOUSE_MOTION),
        (sys::GHOSTTY_MODE_SGR_MOUSE, MODE_SGR_MOUSE),
        (sys::GHOSTTY_MODE_UTF8_MOUSE, MODE_UTF8_MOUSE),
        (sys::GHOSTTY_MODE_BRACKETED_PASTE, MODE_BRACKETED_PASTE),
        (sys::GHOSTTY_MODE_DECCKM, MODE_APP_CURSOR),
        (sys::GHOSTTY_MODE_FOCUS_EVENT, MODE_FOCUS),
        (sys::GHOSTTY_MODE_CURSOR_VISIBLE, MODE_SHOW_CURSOR),
        (sys::GHOSTTY_MODE_KEYPAD_KEYS, MODE_APP_KEYPAD),
    ];
    let mut configs = MODES.map(|(mode, _)| sys::GhosttyTerminalModeConfig { mode, value: false });
    let mut screen: sys::GhosttyTerminalScreen = sys::GHOSTTY_TERMINAL_SCREEN_PRIMARY;
    let mut kitty: sys::GhosttyKittyKeyFlags = 0;
    let mut keys = [sys::GHOSTTY_TERMINAL_DATA_MODE; MODES.len() + 2];
    let mut values = [std::ptr::null_mut::<c_void>(); MODES.len() + 2];
    for (value, config) in values.iter_mut().zip(configs.iter_mut()) {
        *value = (config as *mut sys::GhosttyTerminalModeConfig).cast();
    }
    keys[MODES.len()] = sys::GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN;
    values[MODES.len()] = (&raw mut screen).cast();
    keys[MODES.len() + 1] = sys::GHOSTTY_TERMINAL_DATA_KITTY_KEYBOARD_FLAGS;
    values[MODES.len() + 1] = (&raw mut kitty).cast();
    // SAFETY: each value points to the output type of its key.
    let result = unsafe { sys::ghostty_terminal_get_multi(terminal, keys.len(), keys.as_ptr(), values.as_mut_ptr(), std::ptr::null_mut()) };
    if result != sys::GHOSTTY_SUCCESS {
        // Some values may be unwritten: ask for each one, defaulting what fails.
        for config in &mut configs {
            // SAFETY: DATA_MODE takes a GhosttyTerminalModeConfig.
            config.value = unsafe { terminal_get(terminal, sys::GHOSTTY_TERMINAL_DATA_MODE, *config) }.is_some_and(|c| c.value);
        }
        // SAFETY: as documented for each data kind.
        screen = unsafe { terminal_get(terminal, sys::GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, sys::GHOSTTY_TERMINAL_SCREEN_PRIMARY) }
            .unwrap_or(sys::GHOSTTY_TERMINAL_SCREEN_PRIMARY);
        // SAFETY: as above.
        kitty = unsafe { terminal_get(terminal, sys::GHOSTTY_TERMINAL_DATA_KITTY_KEYBOARD_FLAGS, 0) }.unwrap_or(0);
    }
    let mut bits = 0;
    for ((_, bit), config) in MODES.iter().zip(&configs) {
        if config.value {
            bits |= bit;
        }
    }
    if screen == sys::GHOSTTY_TERMINAL_SCREEN_ALTERNATE {
        bits |= MODE_ALT_SCREEN;
    }
    bits | (kitty as u32 & 0x1f) << MODE_KITTY_SHIFT
}

/// The fields of a packed cell the snapshot needs.
struct CellInfo {
    codepoint: u32,
    tag: sys::GhosttyCellContentTag,
    wide: sys::GhosttyCellWide,
    styled: bool,
    style_id: u16,
    link: bool,
}

impl CellInfo {
    /// A blank, unstyled, narrow cell.
    const BLANK: Self = CellInfo {
        codepoint: 0,
        tag: sys::GHOSTTY_CELL_CONTENT_CODEPOINT,
        wide: sys::GHOSTTY_CELL_WIDE_NARROW,
        styled: false,
        style_id: 0,
        link: false,
    };

    /// The cell's fields, or [`CellInfo::BLANK`] when they cannot be read.
    fn decode(cell: sys::GhosttyCell) -> Self {
        let mut info = Self::BLANK;
        let keys = [
            sys::GHOSTTY_CELL_DATA_CODEPOINT,
            sys::GHOSTTY_CELL_DATA_CONTENT_TAG,
            sys::GHOSTTY_CELL_DATA_WIDE,
            sys::GHOSTTY_CELL_DATA_HAS_STYLING,
            sys::GHOSTTY_CELL_DATA_STYLE_ID,
            sys::GHOSTTY_CELL_DATA_HAS_HYPERLINK,
        ];
        let mut values: [*mut c_void; 6] = [
            (&raw mut info.codepoint).cast(),
            (&raw mut info.tag).cast(),
            (&raw mut info.wide).cast(),
            (&raw mut info.styled).cast(),
            (&raw mut info.style_id).cast(),
            (&raw mut info.link).cast(),
        ];
        // SAFETY: each value points to the output type of its key.
        let result = unsafe { sys::ghostty_cell_get_multi(cell, keys.len(), keys.as_ptr(), values.as_mut_ptr(), std::ptr::null_mut()) };
        if result == sys::GHOSTTY_SUCCESS { info } else { Self::BLANK }
    }
}

/// A cell style in protocol-8 terms.
#[derive(Clone, Copy, Default)]
struct CellStyle {
    fg: u32,
    bg: u32,
    flags: u32,
    underline: u32,
}

/// # Safety
/// `cells` must hold the current row and `x` be within it.
unsafe fn cell_style(cells: sys::GhosttyRenderStateRowCells, x: u16) -> CellStyle {
    let mut style = sys::GhosttyStyle::sized();
    // SAFETY: upheld by the caller; STYLE writes a GhosttyStyle.
    let ok = unsafe {
        sys::ghostty_render_state_row_cells_select(cells, x) == sys::GHOSTTY_SUCCESS
            && sys::ghostty_render_state_row_cells_get(cells, sys::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE, (&raw mut style).cast())
                == sys::GHOSTTY_SUCCESS
    };
    if ok { convert_style(&style) } else { CellStyle::default() }
}

/// The code points after the first of a multi-code-point cell.
///
/// # Safety
/// As for [`cell_style`].
unsafe fn grapheme_extra(cells: sys::GhosttyRenderStateRowCells, x: u16) -> Vec<char> {
    let mut len: u32 = 0;
    // SAFETY: upheld by the caller; LEN writes a u32, BUF writes `len` u32s.
    unsafe {
        if sys::ghostty_render_state_row_cells_select(cells, x) != sys::GHOSTTY_SUCCESS
            || sys::ghostty_render_state_row_cells_get(cells, sys::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN, (&raw mut len).cast())
                != sys::GHOSTTY_SUCCESS
            || len < 2
        {
            return Vec::new();
        }
        let mut buf = vec![0u32; len as usize];
        if sys::ghostty_render_state_row_cells_get(cells, sys::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF, buf.as_mut_ptr().cast())
            != sys::GHOSTTY_SUCCESS
        {
            return Vec::new();
        }
        buf[1..].iter().filter_map(|&cp| char::from_u32(cp)).collect()
    }
}

fn rgb_color(rgb: sys::GhosttyColorRgb) -> u32 {
    COLOR_RGB | (rgb.r as u32) << 16 | (rgb.g as u32) << 8 | rgb.b as u32
}

/// `COLOR_DEFAULT`, palette index + 1 or `COLOR_RGB | rgb`, as alacritty.rs maps colours.
fn style_color(color: &sys::GhosttyStyleColor) -> u32 {
    match color.tag {
        // SAFETY: the tag selects the union field.
        sys::GHOSTTY_STYLE_COLOR_PALETTE => (unsafe { color.value.palette }) as u32 + 1,
        // SAFETY: as above.
        sys::GHOSTTY_STYLE_COLOR_RGB => rgb_color(unsafe { color.value.rgb }),
        _ => COLOR_DEFAULT,
    }
}

fn convert_style(style: &sys::GhosttyStyle) -> CellStyle {
    let mut flags = 0u32;
    for (on, bit) in [
        (style.bold, F_BOLD),
        (style.italic, F_ITALIC),
        (style.inverse, F_INVERSE),
        (style.faint, F_DIM),
        (style.invisible, F_HIDDEN),
        (style.strikethrough, F_STRIKE),
    ] {
        if on {
            flags |= bit as u32;
        }
    }
    if style.overline {
        flags |= F8_OVERLINE;
    }
    if style.blink {
        flags |= F8_BLINK;
    }
    if (sys::GHOSTTY_SGR_UNDERLINE_SINGLE..=sys::GHOSTTY_SGR_UNDERLINE_DASHED).contains(&style.underline) {
        // GhosttySgrUnderline values are the protocol's UL_* values.
        flags |= F_UNDERLINE as u32 | (style.underline as u32) << F8_UL_SHIFT;
    }
    CellStyle { fg: style_color(&style.fg_color), bg: style_color(&style.bg_color), flags, underline: style_color(&style.underline_color) }
}

/// Identical to alacritty.rs: drop trailing default blanks of a row.
fn trim_trailing_blank(runs: &mut Vec<Run8>) {
    while let Some(run) = runs.last_mut() {
        if run.bg != COLOR_DEFAULT || run.flags & (F_INVERSE | F_UNDERLINE | F_STRIKE) as u32 != 0 || run.link != 0 {
            return;
        }
        let trimmed = run.text.trim_end_matches(' ').len();
        if trimmed == 0 {
            runs.pop();
        } else {
            run.text.truncate(trimmed);
            return;
        }
    }
}

/// A key event for Ghostty's encoder.
struct KeySpec {
    key: sys::GhosttyKey,
    /// The text the key produced (already shifted), for character keys.
    text: Option<char>,
    unshifted: u32,
    mods: sys::GhosttyMods,
}

impl KeySpec {
    /// `None` for keys Ghostty has no code for (control characters, F13+).
    fn new(key: Key, mods: u8) -> Option<Self> {
        let mut gmods = 0;
        for (bit, gbit) in [(MOD_SHIFT, sys::GHOSTTY_MODS_SHIFT), (MOD_ALT, sys::GHOSTTY_MODS_ALT), (MOD_CTRL, sys::GHOSTTY_MODS_CTRL)] {
            if mods & bit != 0 {
                gmods |= gbit;
            }
        }
        let functional = |key| Some(KeySpec { key, text: None, unshifted: 0, mods: gmods });
        match key {
            Key::Char(c) if c.is_control() => None,
            // Letters report their lowercase key (kitty); other characters themselves,
            // since the wire format already applied Shift and the layout is unknown.
            Key::Char(c) => Some(KeySpec { key: physical_key(c), text: Some(c), unshifted: c.to_ascii_lowercase() as u32, mods: gmods }),
            Key::Enter => functional(sys::GHOSTTY_KEY_ENTER),
            Key::Tab => functional(sys::GHOSTTY_KEY_TAB),
            Key::BackTab => Some(KeySpec { key: sys::GHOSTTY_KEY_TAB, text: None, unshifted: 0, mods: gmods | sys::GHOSTTY_MODS_SHIFT }),
            Key::Backspace => functional(sys::GHOSTTY_KEY_BACKSPACE),
            Key::Esc => functional(sys::GHOSTTY_KEY_ESCAPE),
            Key::Up => functional(sys::GHOSTTY_KEY_ARROW_UP),
            Key::Down => functional(sys::GHOSTTY_KEY_ARROW_DOWN),
            Key::Left => functional(sys::GHOSTTY_KEY_ARROW_LEFT),
            Key::Right => functional(sys::GHOSTTY_KEY_ARROW_RIGHT),
            Key::Home => functional(sys::GHOSTTY_KEY_HOME),
            Key::End => functional(sys::GHOSTTY_KEY_END),
            Key::PageUp => functional(sys::GHOSTTY_KEY_PAGE_UP),
            Key::PageDown => functional(sys::GHOSTTY_KEY_PAGE_DOWN),
            Key::Insert => functional(sys::GHOSTTY_KEY_INSERT),
            Key::Delete => functional(sys::GHOSTTY_KEY_DELETE),
            Key::F(n @ 1..=12) => functional(sys::GHOSTTY_KEY_F1 + (n - 1) as c_int),
            Key::F(_) => None,
        }
    }
}

/// The US-layout key that types `c` unshifted; other characters have none.
fn physical_key(c: char) -> sys::GhosttyKey {
    match c.to_ascii_lowercase() {
        c @ 'a'..='z' => sys::GHOSTTY_KEY_A + (c as u8 - b'a') as c_int,
        c @ '0'..='9' => sys::GHOSTTY_KEY_DIGIT_0 + (c as u8 - b'0') as c_int,
        '`' => sys::GHOSTTY_KEY_BACKQUOTE,
        '-' => sys::GHOSTTY_KEY_MINUS,
        '=' => sys::GHOSTTY_KEY_EQUAL,
        '[' => sys::GHOSTTY_KEY_BRACKET_LEFT,
        ']' => sys::GHOSTTY_KEY_BRACKET_RIGHT,
        '\\' => sys::GHOSTTY_KEY_BACKSLASH,
        ';' => sys::GHOSTTY_KEY_SEMICOLON,
        '\'' => sys::GHOSTTY_KEY_QUOTE,
        ',' => sys::GHOSTTY_KEY_COMMA,
        '.' => sys::GHOSTTY_KEY_PERIOD,
        '/' => sys::GHOSTTY_KEY_SLASH,
        ' ' => sys::GHOSTTY_KEY_SPACE,
        _ => sys::GHOSTTY_KEY_UNIDENTIFIED,
    }
}

/// Ghostty's legacy encoder extends xterm with fixterms `CSI <code>;<mods> u`
/// (Ctrl+I/M/[, Ctrl+Shift+letter, ...), modifyOtherKeys `CSI 27;<mods>;<code> ~`
/// (Shift/Ctrl+Enter, Ctrl+Tab, ...) and kitty's `CSI 13;<mods> ~` for a
/// modified F3. Without the kitty protocol the bytes go through ConPTY's
/// input parser, which maps only the classic forms to console keys, so these
/// keys keep the `crate::keys` (xterm / Windows Terminal) bytes.
fn is_legacy_extension(bytes: &[u8]) -> bool {
    let Some(body) = bytes.strip_prefix(b"\x1b[") else { return false };
    let Some((&last, params)) = body.split_last() else { return false };
    if !params.iter().all(|b| b.is_ascii_digit() || *b == b';' || *b == b':') {
        return false;
    }
    match last {
        b'u' => true,
        b'~' => params.starts_with(b"27;") || params.starts_with(b"13;"),
        _ => false,
    }
}

// Callbacks: run synchronously inside `vt_write` (or resize/reset); they only
// record into `Shared` and never call back into the terminal for writing.

/// # Safety
/// `userdata` is the emulator's `Shared` and nothing else borrows it now.
unsafe fn shared<'a>(userdata: *mut c_void) -> Option<&'a mut Shared> {
    // SAFETY: upheld by the caller.
    unsafe { userdata.cast::<Shared>().as_mut() }
}

unsafe extern "C" fn on_write_pty(_: sys::GhosttyTerminal, userdata: *mut c_void, data: *const u8, len: usize) {
    // SAFETY: userdata is ours; `data` holds `len` bytes for this call.
    if let Some(shared) = unsafe { shared(userdata) }
        && !data.is_null()
        && len > 0
    {
        shared.pending.push(Pending::Reply(unsafe { std::slice::from_raw_parts(data, len) }.to_vec()));
    }
}

unsafe extern "C" fn on_bell(_: sys::GhosttyTerminal, userdata: *mut c_void) {
    // SAFETY: userdata is ours.
    if let Some(shared) = unsafe { shared(userdata) } {
        shared.pending.push(Pending::Bell);
    }
}

unsafe extern "C" fn on_title_changed(_: sys::GhosttyTerminal, userdata: *mut c_void) {
    // SAFETY: userdata is ours.
    if let Some(shared) = unsafe { shared(userdata) } {
        shared.title_changed = true;
    }
}

unsafe extern "C" fn on_size(_: sys::GhosttyTerminal, userdata: *mut c_void, out: *mut sys::GhosttySizeReportSize) -> bool {
    // SAFETY: userdata is ours.
    let Some(shared) = (unsafe { shared(userdata) }) else { return false };
    if out.is_null() {
        return false;
    }
    let size = sys::GhosttySizeReportSize { rows: shared.rows, columns: shared.cols, cell_width: CELL_WIDTH, cell_height: CELL_HEIGHT };
    // SAFETY: `out` is writable for this call.
    unsafe { out.write(size) };
    true
}

/// DA1 `CSI ? 62 ; 22 c` and DA2 `CSI > 1 ; 10 ; 0 c`, as Ghostty answers.
unsafe extern "C" fn on_device_attributes(_: sys::GhosttyTerminal, _: *mut c_void, out: *mut sys::GhosttyDeviceAttributes) -> bool {
    if out.is_null() {
        return false;
    }
    let mut features = [0u16; 64];
    features[0] = sys::GHOSTTY_DA_FEATURE_ANSI_COLOR;
    let attributes = sys::GhosttyDeviceAttributes {
        primary: sys::GhosttyDeviceAttributesPrimary { conformance_level: sys::GHOSTTY_DA_CONFORMANCE_VT220, features, num_features: 1 },
        secondary: sys::GhosttyDeviceAttributesSecondary { device_type: sys::GHOSTTY_DA_DEVICE_TYPE_VT220, firmware_version: 10, rom_cartridge: 0 },
        tertiary: sys::GhosttyDeviceAttributesTertiary { unit_id: 0 },
    };
    // SAFETY: `out` is writable for this call.
    unsafe { out.write(attributes) };
    true
}

/// OSC 52 (and OSC 1337 / 5522) writes: the text representation becomes a
/// clipboard event, for the standard clipboard only and up to [`CLIPBOARD_MAX_BYTES`].
unsafe extern "C" fn on_clipboard_write(_: sys::GhosttyTerminal, userdata: *mut c_void, write: *const sys::GhosttyClipboardWrite) {
    // SAFETY: userdata is ours; the request is borrowed for this call.
    let (Some(shared), Some(request)) = (unsafe { shared(userdata) }, unsafe { write.as_ref() }) else { return };
    let answer = |result| {
        if let Some(reply) = request.reply {
            let mut answer = sys::GhosttyClipboardWriteReply::sized();
            answer.result = result;
            // SAFETY: answered during the callback with the request it came with.
            unsafe { reply(write, &answer) };
        }
    };
    if request.size < std::mem::size_of::<sys::GhosttyClipboardWrite>() || request.contents.is_null() || request.contents_len == 0 {
        return;
    }
    if request.location != sys::GHOSTTY_CLIPBOARD_LOCATION_STANDARD {
        return answer(sys::GHOSTTY_CLIPBOARD_WRITE_RESULT_DENIED);
    }
    // SAFETY: `contents_len` entries whose strings live for this call.
    let contents = unsafe { std::slice::from_raw_parts(request.contents, request.contents_len) };
    let mime = |c: &&sys::GhosttyClipboardContent, prefix: &[u8]| unsafe { c.mime.as_bytes() }.starts_with(prefix);
    let Some(text) = contents.iter().find(|c| mime(c, b"text/plain")).or_else(|| contents.iter().find(|c| mime(c, b"text/"))) else {
        return;
    };
    // SAFETY: as above.
    let data = unsafe { text.data.as_bytes() };
    if data.len() > CLIPBOARD_MAX_BYTES {
        return answer(sys::GHOSTTY_CLIPBOARD_WRITE_RESULT_DENIED);
    }
    shared.pending.push(Pending::Clipboard(String::from_utf8_lossy(data).into_owned()));
    answer(sys::GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS);
}

unsafe extern "C" fn on_notification(
    _: sys::GhosttyTerminal,
    userdata: *mut c_void,
    notification: *const sys::GhosttyTerminalDesktopNotification,
) {
    // SAFETY: userdata is ours; the notification is borrowed for this call.
    let (Some(shared), Some(notification)) = (unsafe { shared(userdata) }, unsafe { notification.as_ref() }) else { return };
    // Ghostty reads an OSC 9;4 progress report with an invalid state as an
    // OSC 9 notification with body "4;<state>"; it is neither.
    // SAFETY: the body lives for this call.
    if notification.size >= std::mem::size_of::<sys::GhosttyTerminalDesktopNotification>()
        && unsafe { notification.title.as_bytes() }.is_empty()
        && unsafe { notification.body.as_bytes() }.starts_with(b"4;")
    {
        return;
    }
    shared.pending.push(Pending::Notify);
}

unsafe extern "C" fn on_progress(_: sys::GhosttyTerminal, userdata: *mut c_void, report: *const sys::GhosttyTerminalProgressReport) {
    // SAFETY: userdata is ours; the report is borrowed for this call.
    let (Some(shared), Some(report)) = (unsafe { shared(userdata) }, unsafe { report.as_ref() }) else { return };
    if report.size >= std::mem::size_of::<sys::GhosttyTerminalProgressReport>()
        && (sys::GHOSTTY_TERMINAL_PROGRESS_STATE_REMOVE..=sys::GHOSTTY_TERMINAL_PROGRESS_STATE_PAUSE).contains(&report.state)
    {
        shared.pending.push(Pending::Progress(report.state as u8));
    }
}

/// A hold begins with the screen the program wants shown: capture it (and
/// the [`View`] that goes with it) now.
unsafe extern "C" fn on_render_hold(terminal: sys::GhosttyTerminal, userdata: *mut c_void, held: bool) {
    // SAFETY: userdata is ours.
    let Some(shared) = (unsafe { shared(userdata) }) else { return };
    if held {
        // SAFETY: reading the terminal from a callback is allowed (only writes re-enter).
        if unsafe { sys::ghostty_render_state_update(shared.render, terminal) } != sys::GHOSTTY_SUCCESS {
            // No frame to hold: keep showing the live screen.
            return;
        }
        shared.held = View::read(terminal);
        shared.hold = Some(Instant::now());
    } else {
        shared.hold = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulator::alacritty::AlacrittyEmulator;
    use crate::emulator::{create, feed_collect};

    fn config(cols: u16, rows: u16, kitty_keyboard: bool) -> Config {
        Config { cols, rows, scrollback: 1000, kitty_keyboard }
    }

    fn emulator(cols: u16, rows: u16) -> GhosttyEmulator {
        GhosttyEmulator::new(&config(cols, rows, false)).expect("ghostty engine")
    }

    // ---- The alacritty.rs tests, same assertions ----

    #[test]
    fn snapshot_preserves_colors_wide_chars_and_cursor() {
        let mut emu = emulator(20, 3);
        feed_collect(&mut emu, "\x1b[31mred\x1b[0m \u{4e2d}!\r\n\x1b[48;2;1;2;3mbg\x1b[0m".as_bytes());
        let snap = emu.snapshot();
        let first = snap.row_v7(0);
        assert_eq!(first[0], Run { col: 0, text: "red".into(), fg: 2, bg: 0, flags: 0, link: None });
        assert_eq!(first[1].col, 3);
        assert_eq!(first[1].text, " \u{4e2d}!");
        assert_eq!(snap.row_v7(1)[0].bg, COLOR_RGB | 0x010203);
        assert_eq!(snap.cursor, Some((2, 1)));
        assert!(snap.lines[2].is_empty(), "blank rows carry no runs");
        let wide: Vec<_> = snap.lines[0].iter().filter(|r| r.wide).collect();
        assert_eq!((wide.len(), wide[0].text.as_str(), wide[0].col, wide[0].width()), (1, "\u{4e2d}", 4, 2));
    }

    #[test]
    fn grapheme_clusters_and_underline_styles_reach_protocol_8() {
        let mut emu = emulator(20, 2);
        feed_collect(&mut emu, "e\u{301}x \x1b[4:3mcurly\x1b[0m".as_bytes());
        let snap = emu.snapshot();
        let row = &snap.lines[0];
        assert_eq!(row[0].cells()[..2], ["e\u{301}", "x"], "combining mark stays in its cell");
        let curly = row.iter().find(|r| r.text == "curly").expect("styled run");
        assert_eq!((curly.flags & F8_UL_MASK) >> F8_UL_SHIFT, UL_CURLY);
        assert_ne!(curly.flags & F_UNDERLINE as u32, 0);
    }

    #[test]
    fn progress_and_notifications_arrive_across_reads() {
        let mut emu = emulator(20, 2);
        assert_eq!(feed_collect(&mut emu, b"text \x1b]9;4;3;0\x07 more").progress, vec![3]);
        assert_eq!(feed_collect(&mut emu, b"\x1b]9;4;").progress, Vec::<u8>::new());
        assert_eq!(feed_collect(&mut emu, b"0;0\x1b\\").progress, vec![0], "split read, ST terminator");
        let events = feed_collect(&mut emu, b"\x1b]777;notify;T;B\x07\x1b]0;title\x07\x1b]9;4;9\x07");
        assert_eq!((events.notifications, events.progress.as_slice()), (1, &[][..]), "invalid progress state ignored");
    }

    #[test]
    fn osc8_hyperlinks_become_link_runs() {
        let mut emu = emulator(40, 2);
        feed_collect(&mut emu, b"see \x1b]8;;https://github.com/pr/1\x1b\\PR 1\x1b]8;;\x1b\\ now");
        let snap = emu.snapshot();
        let line = snap.row_v7(0);
        let linked: Vec<_> = line.iter().filter(|r| r.link.is_some()).collect();
        assert_eq!(linked.len(), 1);
        assert_eq!((linked[0].text.as_str(), linked[0].link.as_deref()), ("PR 1", Some("https://github.com/pr/1")));
        assert!(line.last().unwrap().text.ends_with("now"));
        assert_eq!(snap.links, vec!["https://github.com/pr/1".to_owned()]);
    }

    #[test]
    fn answers_cursor_position_queries_required_by_conpty() {
        let mut emu = emulator(20, 5);
        let events = feed_collect(&mut emu, b"ab\x1b[6n");
        assert_eq!(events.replies, vec![b"\x1b[1;3R".to_vec()]);
        let events = feed_collect(&mut emu, b"\x1b]11;?\x07");
        let reply = String::from_utf8(events.replies[0].clone()).unwrap();
        assert!(reply.starts_with("\x1b]11;rgb:"), "background color query answered: {reply:?}");
    }

    #[test]
    fn titles_bells_and_progress_are_events() {
        let mut emu = emulator(20, 5);
        let events = feed_collect(&mut emu, b"\x1b]0;hello\x07\x07\x1b]9;4;3;0\x07\x1b]777;notify;a;b\x07");
        assert_eq!(events.title, Some(Some("hello".into())));
        assert_eq!((events.bells, events.progress.as_slice(), events.notifications), (1, &[3u8][..], 1));
    }

    #[test]
    fn hidden_cursor_and_scrollback_are_reflected() {
        let mut emu = emulator(10, 3);
        for i in 0..10 {
            feed_collect(&mut emu, format!("line{i}\r\n").as_bytes());
        }
        feed_collect(&mut emu, b"\x1b[?25l");
        assert_eq!(emu.snapshot().cursor, None);
        emu.scroll(Scroll::Delta(3));
        let snap = emu.snapshot();
        assert_eq!(snap.display_offset, 3);
        assert!(snap.history >= 3);
        assert_eq!(snap.lines[0][0].text, "line5");
    }

    #[test]
    fn modes_follow_the_program() {
        let mut emu = emulator(10, 3);
        feed_collect(&mut emu, b"\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b[?1h");
        let m = emu.modes();
        for bit in [MODE_ALT_SCREEN, MODE_MOUSE, MODE_SGR_MOUSE, MODE_BRACKETED_PASTE, MODE_APP_CURSOR] {
            assert_ne!(m & bit, 0, "mode bit {bit:#x}");
        }
    }

    #[test]
    fn default_colors_match_the_alacritty_engine() {
        for query in [&b"\x1b]4;1;?\x07"[..], b"\x1b]4;16;?\x07", b"\x1b]4;231;?\x07", b"\x1b]4;232;?\x1b\\", b"\x1b]10;?\x07", b"\x1b]11;?\x07"] {
            let mut alacritty = AlacrittyEmulator::new(&config(20, 5, false));
            let mut ghostty = emulator(20, 5);
            assert_eq!(feed_collect(&mut ghostty, query).replies, feed_collect(&mut alacritty, query).replies, "{query:?}");
        }
    }

    // ---- Ghostty specifics ----

    #[test]
    fn the_engine_is_created_through_the_factory() {
        let mut emu = create(Engine::Ghostty, &config(20, 4, true));
        assert_eq!(emu.engine(), Engine::Ghostty);
        feed_collect(emu.as_mut(), b"hi");
        assert_eq!(emu.snapshot().text().lines().next(), Some("hi"));
    }

    #[test]
    fn modes_cover_mouse_formats_focus_keypad_and_kitty_flags() {
        let mut emu = emulator(10, 3);
        assert_eq!(emu.modes(), MODE_SHOW_CURSOR);
        feed_collect(&mut emu, b"\x1b[?1002h\x1b[?1005h\x1b[?1004h\x1b=\x1b[>5u");
        let m = emu.modes();
        assert_eq!(m & (MODE_MOUSE | MODE_MOUSE_DRAG | MODE_UTF8_MOUSE | MODE_FOCUS | MODE_APP_KEYPAD), MODE_MOUSE | MODE_MOUSE_DRAG | MODE_UTF8_MOUSE | MODE_FOCUS | MODE_APP_KEYPAD);
        assert_eq!(m >> MODE_KITTY_SHIFT, 5, "kitty flags");
        feed_collect(&mut emu, b"\x1b[?1002l\x1b[?1003h\x1b[<u");
        let m = emu.modes();
        assert_eq!(m & (MODE_MOUSE | MODE_MOUSE_MOTION | MODE_MOUSE_DRAG), MODE_MOUSE | MODE_MOUSE_MOTION);
        assert_eq!(m >> MODE_KITTY_SHIFT, 0, "flags popped");
    }

    #[test]
    fn clipboard_writes_are_events_and_reads_are_refused() {
        let mut emu = emulator(20, 3);
        let events = feed_collect(&mut emu, b"\x1b]52;c;aGVsbG8gd29ybGQ=\x07");
        assert_eq!(events.clipboard, vec!["hello world".to_owned()]);
        let events = feed_collect(&mut emu, b"\x1b]52;c;?\x07\x1b]52;p;?\x1b\\");
        assert!(events.replies.is_empty() && events.clipboard.is_empty(), "no clipboard contents leave: {events:?}");
    }

    #[test]
    fn clipboard_writes_are_capped_and_limited_to_the_standard_clipboard() {
        use crate::emulator::alacritty::tests::base64;
        let mut emu = emulator(20, 3);
        let osc = |target: &str, text: &[u8]| format!("\x1b]52;{target};{}\x07", base64(text)).into_bytes();
        assert_eq!(feed_collect(&mut emu, &osc("c", &[b'a'; 8192])).clipboard[0].len(), 8192);
        assert!(feed_collect(&mut emu, &osc("p", b"primary")).clipboard.is_empty(), "primary writes are refused");
        assert!(feed_collect(&mut emu, &osc("s", b"select")).clipboard.is_empty(), "selection writes are refused");
        let over = vec![b'a'; CLIPBOARD_MAX_BYTES + 1];
        assert!(feed_collect(&mut emu, &osc("c", &over)).clipboard.is_empty(), "oversized writes are dropped");
        assert_eq!(feed_collect(&mut emu, &osc("c", b"after")).clipboard, ["after"], "the parser recovers");
        // SAFETY: DATA_CLIPBOARD_WRITE_MAX_BYTES yields a size_t.
        let limit = unsafe { emu.get(sys::GHOSTTY_TERMINAL_DATA_CLIPBOARD_WRITE_MAX_BYTES, 0usize) }.expect("limit");
        assert_eq!(limit, CLIPBOARD_MAX_BYTES, "OSC 5522 transactions are capped as well");
    }

    #[test]
    fn adjacent_links_with_the_same_style_stay_distinct() {
        let mut emu = emulator(40, 2);
        feed_collect(&mut emu, b"\x1b]8;;https://a.example\x07aa\x1b]8;;https://b.example\x07bb\x1b]8;;https://a.example\x07cc\x1b]8;;\x07");
        let snap = emu.snapshot();
        assert_eq!(snap.links, ["https://a.example", "https://b.example"]);
        let ids: Vec<(String, u32)> = snap.lines[0].iter().map(|r| (r.text.clone(), r.link)).collect();
        assert_eq!(ids, [("aa".to_owned(), 1), ("bb".to_owned(), 2), ("cc".to_owned(), 1)]);
    }

    #[test]
    fn many_links_map_to_their_targets() {
        let mut emu = emulator(200, 30);
        let mut stream = String::new();
        for i in 0..200 {
            stream.push_str(&format!("\x1b]8;;https://x.example/{i}\x07{}\x1b]8;;\x07 ", i % 10));
        }
        feed_collect(&mut emu, stream.as_bytes());
        let snap = emu.snapshot();
        assert_eq!(snap.links.len(), 200);
        let mut linked = 0;
        for run in snap.lines.iter().flatten().filter(|r| r.link != 0) {
            let target: usize = snap.links[run.link as usize - 1].rsplit('/').next().unwrap().parse().unwrap();
            assert_eq!(run.text, (target % 10).to_string(), "link {} points at its own target", run.link);
            linked += 1;
        }
        assert_eq!(linked, 200);
    }

    #[test]
    fn a_held_snapshot_does_not_mix_in_live_links_or_modes() {
        let mut emu = emulator(30, 3);
        feed_collect(&mut emu, b"plain");
        feed_collect(&mut emu, b"\x1b[?2026h\x1b[?1049h\x1b[H\x1b]8;;https://live.example\x07link\x1b]8;;\x07\x1b[?1000h");
        assert!(emu.sync_deadline().is_some());
        assert_ne!(emu.modes() & MODE_MOUSE, 0, "the live terminal has the new modes");
        let held = emu.snapshot();
        assert_eq!(held.text().lines().next(), Some("plain"));
        assert!(held.links.is_empty() && held.lines[0].iter().all(|r| r.link == 0), "no live links in the held frame");
        assert_eq!(held.modes & (MODE_MOUSE | MODE_ALT_SCREEN), 0, "modes as of the held frame");
        feed_collect(&mut emu, b"\x1b[?2026l");
        let live = emu.snapshot();
        assert_eq!(live.links, ["https://live.example"]);
        assert_eq!(live.modes & (MODE_MOUSE | MODE_ALT_SCREEN), MODE_MOUSE | MODE_ALT_SCREEN);
    }

    #[test]
    fn full_reset_clears_title_and_progress() {
        let mut emu = emulator(20, 3);
        feed_collect(&mut emu, b"\x1b]2;busy\x07\x1b]9;4;3\x07");
        let events = feed_collect(&mut emu, b"\x1bc");
        assert_eq!((events.title, events.progress), (Some(None), vec![0]));
    }

    #[test]
    fn synchronized_output_shows_the_frame_from_before_the_update() {
        let mut emu = emulator(20, 3);
        feed_collect(&mut emu, b"before");
        assert_eq!(emu.sync_deadline(), None);
        feed_collect(&mut emu, b"\x1b[?2026h\x1b[2J\x1b[Hhalf");
        let deadline = emu.sync_deadline().expect("hold active");
        assert!(deadline <= Instant::now() + SYNC_TIMEOUT);
        assert_eq!(emu.snapshot().text().lines().next(), Some("before"), "mid-update snapshot shows the held frame");
        feed_collect(&mut emu, b" done\x1b[?2026l");
        assert_eq!(emu.sync_deadline(), None);
        assert_eq!(emu.snapshot().text().lines().next(), Some("half done"));
        // An update whose end marker never comes is ended by the caller.
        feed_collect(&mut emu, b"\x1b[?2026h\x1b[Hstuck");
        assert!(emu.sync_deadline().is_some());
        let mut events = crate::emulator::Collected::default();
        emu.end_sync(&mut events);
        assert_eq!(emu.sync_deadline(), None);
        assert_eq!(emu.snapshot().text().lines().next(), Some("stuckdone"));
        // A resize ends a hold too.
        feed_collect(&mut emu, b"\x1b[?2026h");
        emu.resize(30, 4);
        assert_eq!(emu.sync_deadline(), None);
    }

    #[test]
    fn resize_scroll_and_size_reports_track_the_grid() {
        let mut emu = emulator(20, 3);
        for i in 0..20 {
            feed_collect(&mut emu, format!("row{i}\r\n").as_bytes());
        }
        emu.resize(30, 5);
        let snap = emu.snapshot();
        assert_eq!((snap.cols, snap.rows, snap.lines.len()), (30, 5, 5));
        assert_eq!(feed_collect(&mut emu, b"\x1b[18t").replies, vec![b"\x1b[8;5;30t".to_vec()]);
        emu.scroll(Scroll::Top);
        assert_eq!(emu.display_offset(), emu.snapshot().history);
        assert_eq!(emu.snapshot().cursor, None, "no cursor while scrolled back");
        emu.scroll(Scroll::Delta(-2));
        assert_eq!(emu.display_offset(), emu.snapshot().history - 2);
        emu.scroll(Scroll::Bottom);
        assert_eq!(emu.display_offset(), 0);
        assert!(emu.snapshot().cursor.is_some());
    }

    #[test]
    fn malformed_input_never_panics() {
        let mut emu = emulator(17, 7);
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut chunk = Vec::new();
        for round in 0..400 {
            chunk.clear();
            for _ in 0..(round % 97 + 1) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                // Bias towards escape-sequence bytes.
                let byte = match seed % 8 {
                    0 => 0x1b,
                    1 => b'[',
                    2 => b';',
                    3 => b'0' + (seed >> 8) as u8 % 10,
                    _ => (seed >> 16) as u8,
                };
                chunk.push(byte);
            }
            feed_collect(&mut emu, &chunk);
            if round % 50 == 0 {
                emu.resize(5 + (round % 40) as u16, 2 + (round % 9) as u16);
                let _ = emu.snapshot();
            }
        }
        for nasty in [&b"\x1b[999999999999999999;99999999999H"[..], b"\x1b]8;;\x07", b"\x1bP\x1b\\", &[0xff, 0xfe, 0xc0, 0x80][..], b"\x1b]52;c;!!!\x07"] {
            feed_collect(&mut emu, nasty);
        }
        let _ = (emu.snapshot(), emu.modes(), emu.encode(&Input::Key { key: Key::Char('x'), mods: MOD_CTRL }));
        // Creating and dropping many engines frees everything they own.
        for _ in 0..50 {
            let mut e = emulator(80, 24);
            feed_collect(&mut e, b"\x1b[?2026h\x1b]8;;x\x07link");
        }
    }

    #[test]
    fn paste_is_bracketed_and_cannot_terminate_early() {
        let mut emu = emulator(20, 3);
        assert_eq!(emu.encode(&Input::Paste("a\nb".into())), b"a\rb");
        assert_eq!(emu.encode(&Input::Paste("a\r\nb\rc".into())), b"a\rb\rc");
        feed_collect(&mut emu, b"\x1b[?2004h");
        let encoded = emu.encode(&Input::Paste("a\x1b[201~b\nc".into()));
        assert_eq!(encoded, b"\x1b[200~ab\nc\x1b[201~");
        assert_eq!(emu.encode(&Input::Paste("x\x1b[200~\x1b[201~y".into())), b"\x1b[200~xy\x1b[201~");
        // Reviewed difference: Ghostty also blanks other control bytes keys.rs passes on.
        assert_eq!(emu.encode(&Input::Paste("a\x1bb\x03\tc".into())), b"\x1b[200~a b \tc\x1b[201~");
        assert_eq!(emu.encode(&Input::Raw(b"\x1b[A".to_vec())), b"\x1b[A");
    }

    #[test]
    fn focus_reports_only_when_requested() {
        let mut emu = emulator(20, 3);
        assert!(emu.encode(&Input::Focus(true)).is_empty());
        feed_collect(&mut emu, b"\x1b[?1004h");
        assert_eq!(emu.encode(&Input::Focus(true)), b"\x1b[I");
        assert_eq!(emu.encode(&Input::Focus(false)), b"\x1b[O");
    }

    #[test]
    fn wheel_and_clicks_follow_the_keys_rules() {
        let setups: [&[u8]; 8] = [
            b"",
            b"\x1b[?1049h",
            b"\x1b[?1000h",
            b"\x1b[?1000h\x1b[?1006h",
            b"\x1b[?1000h\x1b[?1005h",
            b"\x1b[?1049h\x1b[?1002h\x1b[?1006h",
            b"\x1b[?1003h",
            b"\x1b[?9h",
        ];
        for setup in setups {
            let mut ghostty = emulator(300, 40);
            let mut alacritty = AlacrittyEmulator::new(&config(300, 40, false));
            feed_collect(&mut ghostty, setup);
            feed_collect(&mut alacritty, setup);
            let modes = ghostty.modes();
            for (col, row) in [(0, 0), (4, 2), (250, 39)] {
                for mods in [0, MOD_SHIFT, MOD_ALT, MOD_CTRL, MOD_CTRL | MOD_ALT] {
                    for up in [true, false] {
                        let wheel = ghostty.encode_wheel(up, col, row, mods);
                        assert_eq!(wheel, crate::keys::encode_wheel(up, col, row, mods, modes), "{setup:?} wheel");
                        if setup != b"\x1b[?9h" {
                            assert_eq!(wheel, alacritty.encode_wheel(up, col, row, mods), "{setup:?} wheel vs alacritty");
                        }
                    }
                    for button in 0..4 {
                        let click = ghostty.encode_click(button, col, row, mods);
                        assert_eq!(click, crate::keys::encode_click(button, col, row, mods, modes), "{setup:?} click");
                        if setup != b"\x1b[?9h" {
                            assert_eq!(click, alacritty.encode_click(button, col, row, mods), "{setup:?} click vs alacritty");
                        }
                    }
                }
            }
        }
        // Reviewed difference: X10 mouse (DECSET 9) counts as mouse reporting; alacritty ignores it.
        let mut emu = emulator(20, 3);
        feed_collect(&mut emu, b"\x1b[?9h");
        assert_eq!(emu.encode_click(0, 0, 0, 0).as_deref(), Some(&b"\x1b[M !!\x1b[M#!!"[..]));
    }

    /// Every key the TUI forwards (canvas-core `input::to_wire`): characters
    /// with Shift already applied (AltGr and dead-key results arrive as plain
    /// characters), Ctrl/Alt chords, and the named keys with modifiers.
    fn tui_keys() -> Vec<(Key, u8)> {
        let mut keys = Vec::new();
        for c in ['a', 'z', 'A', 'Z', '0', '9', ' ', '@', '#', '~', '/', '-', '_', '[', ']', '\\', '^', '?', '{', '|', '\u{e9}', '\u{20ac}', '\u{df}', '\u{4e2d}'] {
            keys.push((Key::Char(c), 0));
        }
        for c in 'a'..='z' {
            keys.push((Key::Char(c), MOD_CTRL));
            keys.push((Key::Char(c), MOD_ALT));
            keys.push((Key::Char(c), MOD_CTRL | MOD_ALT));
        }
        for c in ['@', ' ', '2', '3', '4', '5', '6', '7', '8', '[', '\\', ']', '^', '_', '-', '?', '/', '~'] {
            keys.push((Key::Char(c), MOD_CTRL));
        }
        for c in ['C', 'X', '1', '.', '\u{e9}'] {
            keys.push((Key::Char(c), MOD_CTRL));
            keys.push((Key::Char(c), MOD_ALT));
        }
        let named = [
            Key::Enter, Key::Tab, Key::BackTab, Key::Backspace, Key::Esc, Key::Up, Key::Down, Key::Left, Key::Right,
            Key::Home, Key::End, Key::PageUp, Key::PageDown, Key::Insert, Key::Delete,
        ];
        for key in named.into_iter().chain((1..=12).map(Key::F)) {
            for mods in [0, MOD_SHIFT, MOD_CTRL, MOD_ALT, MOD_CTRL | MOD_SHIFT, MOD_ALT | MOD_SHIFT, MOD_CTRL | MOD_ALT] {
                keys.push((key, mods));
            }
        }
        keys
    }

    fn keyed(setup: &[u8]) -> GhosttyEmulator {
        let mut emu = GhosttyEmulator::new(&config(80, 24, true)).expect("ghostty engine");
        feed_collect(&mut emu, setup);
        emu
    }

    #[test]
    fn legacy_key_table_matches_keys_rs_with_reviewed_differences() {
        // Reviewed: Ghostty sends xterm's C0 bytes where keys.rs sends the bare character.
        let differences: &[(Key, u8, &[u8])] = &[
            (Key::Char('/'), MOD_CTRL, b"\x1f"),
            (Key::Char('~'), MOD_CTRL, b"\x1e"),
        ];
        for setup in [&b""[..], b"\x1b[?1h", b"\x1b[?1h\x1b="] {
            let mut emu = keyed(setup);
            let modes = emu.modes();
            assert_eq!(modes >> MODE_KITTY_SHIFT, 0);
            for (key, mods) in tui_keys() {
                let input = Input::Key { key, mods };
                let expected = match differences.iter().find(|(k, m, _)| *k == key && *m == mods) {
                    Some((_, _, bytes)) => bytes.to_vec(),
                    None => crate::keys::encode(&input, modes),
                };
                assert_eq!(emu.encode(&input), expected, "{key:?} mods {mods} after {setup:?}");
            }
        }
        // The keys.rs expectations themselves.
        let mut emu = keyed(b"");
        let key = |emu: &mut GhosttyEmulator, key, mods| emu.encode(&Input::Key { key, mods });
        assert_eq!(key(&mut emu, Key::Char('c'), MOD_CTRL), [3]);
        assert_eq!(key(&mut emu, Key::Char('C'), MOD_CTRL), [3], "Ctrl+Shift+C as the TUI sends it");
        assert_eq!(key(&mut emu, Key::Char('x'), MOD_ALT), b"\x1bx");
        assert_eq!(key(&mut emu, Key::Char('@'), 0), b"@", "AltGr result");
        assert_eq!(key(&mut emu, Key::Char('\u{e9}'), 0), "\u{e9}".as_bytes(), "dead-key result");
        assert_eq!(key(&mut emu, Key::Enter, MOD_SHIFT), b"\x1b\r");
        assert_eq!(key(&mut emu, Key::Backspace, MOD_CTRL), [0x08]);
        assert_eq!(key(&mut emu, Key::Esc, 0), [0x1b]);
        assert_eq!(key(&mut emu, Key::Char('i'), MOD_CTRL), b"\t", "ConPTY-compatible, not fixterms CSI u");
        assert_eq!(key(&mut emu, Key::F(3), MOD_CTRL), b"\x1b[1;5R", "xterm form, not CSI 13;5~");
        assert_eq!(key(&mut emu, Key::Up, 0), b"\x1b[A");
        let mut app = keyed(b"\x1b[?1h");
        assert_eq!(key(&mut app, Key::Up, 0), b"\x1bOA");
        assert_eq!(key(&mut app, Key::Up, MOD_CTRL), b"\x1b[1;5A");
    }

    #[test]
    fn kitty_key_table_follows_the_protocol() {
        // With the kitty protocol on, Ghostty's encoder is authoritative.
        // Reviewed differences from keys.rs (which only special-cases
        // Shift/Ctrl+Enter and Esc): modified text keys, Tab, Backspace and
        // Esc become CSI u, and F1-F4 drop their SS3 form.
        let table: &[(Key, u8, &[u8])] = &[
            (Key::Char('a'), 0, b"a"),
            (Key::Char('A'), 0, b"A"),
            (Key::Char('A'), MOD_SHIFT, b"A"),
            (Key::Char('@'), 0, b"@"),
            (Key::Char('\u{e9}'), 0, "\u{e9}".as_bytes()),
            (Key::Char('c'), MOD_CTRL, b"\x1b[99;5u"),
            (Key::Char('C'), MOD_CTRL, b"\x1b[99;5u"),
            (Key::Char('x'), MOD_ALT, b"\x1b[120;3u"),
            (Key::Char('c'), MOD_CTRL | MOD_ALT, b"\x1b[99;7u"),
            (Key::Char(' '), MOD_CTRL, b"\x1b[32;5u"),
            (Key::Char('['), MOD_CTRL, b"\x1b[91;5u"),
            (Key::Char('\u{e9}'), MOD_ALT, b"\x1b[233;3u"),
            (Key::Enter, 0, b"\r"),
            (Key::Enter, MOD_SHIFT, b"\x1b[13;2u"),
            (Key::Enter, MOD_CTRL, b"\x1b[13;5u"),
            (Key::Enter, MOD_ALT, b"\x1b[13;3u"),
            (Key::Tab, 0, b"\t"),
            (Key::Tab, MOD_SHIFT, b"\x1b[9;2u"),
            (Key::BackTab, 0, b"\x1b[9;2u"),
            (Key::Backspace, 0, b"\x7f"),
            (Key::Backspace, MOD_CTRL, b"\x1b[127;5u"),
            (Key::Esc, 0, b"\x1b[27u"),
            (Key::Esc, MOD_SHIFT, b"\x1b[27;2u"),
            (Key::Up, 0, b"\x1b[A"),
            (Key::Left, MOD_CTRL, b"\x1b[1;5D"),
            (Key::Home, 0, b"\x1b[H"),
            (Key::End, MOD_SHIFT, b"\x1b[1;2F"),
            (Key::PageUp, 0, b"\x1b[5~"),
            (Key::PageDown, MOD_ALT, b"\x1b[6;3~"),
            (Key::Insert, 0, b"\x1b[2~"),
            (Key::Delete, MOD_CTRL, b"\x1b[3;5~"),
            (Key::F(1), 0, b"\x1b[P"),
            (Key::F(2), MOD_SHIFT, b"\x1b[1;2Q"),
            (Key::F(3), 0, b"\x1b[13~"),
            (Key::F(4), 0, b"\x1b[S"),
            (Key::F(5), 0, b"\x1b[15~"),
            (Key::F(12), MOD_CTRL, b"\x1b[24;5~"),
        ];
        let mut emu = keyed(b"\x1b[>1u");
        assert_eq!(emu.modes() & MODE_KITTY_DISAMBIGUATE, MODE_KITTY_DISAMBIGUATE);
        for &(key, mods, expected) in table {
            assert_eq!(emu.encode(&Input::Key { key, mods }), expected, "{key:?} mods {mods}");
        }
        // keys.rs's own kitty expectations hold.
        let modes = emu.modes();
        for (key, mods) in [(Key::Enter, MOD_SHIFT), (Key::Esc, 0)] {
            let input = Input::Key { key, mods };
            assert_eq!(emu.encode(&input), crate::keys::encode(&input, modes));
        }
        // Every forwarded key encodes to text or a well-formed escape sequence.
        for (key, mods) in tui_keys() {
            let bytes = emu.encode(&Input::Key { key, mods });
            assert!(!bytes.is_empty() && (bytes[0] != 0x1b || bytes.len() > 1), "{key:?} mods {mods}: {bytes:?}");
        }
        // Report-all-keys mode encodes even plain text as CSI u.
        let mut all = keyed(b"\x1b[>9u");
        assert_eq!(all.encode(&Input::Key { key: Key::Char('a'), mods: 0 }), b"\x1b[97u");
        assert_eq!(all.encode(&Input::Key { key: Key::Enter, mods: 0 }), b"\x1b[13u");
    }

    #[test]
    fn legacy_extensions_are_recognized() {
        assert!(is_legacy_extension(b"\x1b[105;5u"));
        assert!(is_legacy_extension(b"\x1b[27;2;13~"));
        assert!(is_legacy_extension(b"\x1b[13;5~"));
        for plain in [&b"\x1b[13~"[..], b"\x1b[3;5~", b"\x1b[1;5A", b"\x1bOA", b"\x1b\r", b"a", b"\x1b[200~x"] {
            assert!(!is_legacy_extension(plain), "{plain:?}");
        }
    }

    // ---- Cross-engine comparison ----

    fn both(cols: u16, rows: u16, bytes: &[u8]) -> (Snapshot, Snapshot) {
        let mut alacritty = AlacrittyEmulator::new(&config(cols, rows, false));
        let mut ghostty = emulator(cols, rows);
        feed_collect(&mut alacritty, bytes);
        feed_collect(&mut ghostty, bytes);
        (alacritty.snapshot(), ghostty.snapshot())
    }

    /// Byte streams both engines must render identically: same text, same
    /// protocol-7 rows, same protocol-8 cells and the same cursor.
    #[test]
    fn engines_agree_on_the_common_corpus() {
        let corpus: &[(&str, &str)] = &[
            ("sgr16", "\x1b[31mred\x1b[32mgreen\x1b[0m \x1b[91mbright\x1b[1;34mbold blue\x1b[0m"),
            ("sgr256", "\x1b[38;5;196mA\x1b[48;5;21mB\x1b[38;5;16;48;5;255mC\x1b[0m"),
            ("truecolor", "\x1b[38;2;1;2;3mfg\x1b[48;2;4;5;6mbg\x1b[38:2::7:8:9mcolon\x1b[0m"),
            ("attributes", "\x1b[1mb\x1b[22;3mi\x1b[23;4mu\x1b[24;7mr\x1b[27;9ms\x1b[29;2md\x1b[22;8mh\x1b[0m end"),
            ("underlines", "\x1b[4:1ms\x1b[4:2md\x1b[4:3mc\x1b[4:4mo\x1b[4:5ma\x1b[24mx\x1b[0m \x1b[4;58;5;3mi\x1b[58;2;1;2;3mr\x1b[0m"),
            ("cjk", "\u{4e2d}\u{6587}\u{5b57}\u{7b26} ok \u{3042}\u{ff21}"),
            ("emoji", "\u{1f600} x \u{1f44d}\u{1f3fd} \u{2764}\u{fe0f}!"),
            ("zwj", "\u{1f469}\u{200d}\u{1f4bb}! \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}"),
            ("combining", "e\u{301}x a\u{308}\u{301} \u{1100}\u{1161}\u{11a8} n\u{303}"),
            ("osc8", "\x1b]8;;https://a.example/1\x1b\\one\x1b]8;;\x1b\\ \x1b]8;id=x;https://b.example/2\x07two\x1b]8;;\x07 \x1b]8;;https://a.example/1\x1b\\again\x1b]8;;\x1b\\"),
            ("cursor", "abc\x1b[2;5Hxy\x1b[1;1Hz\x1b[3C!\x1b[3;2H\x1b[1Kq\x1b[A\x1b[2D<"),
            ("erase", "0123456789\r\x1b[5C\x1b[K\x1b[2;1Hab\x1b[44m\x1b[K\x1b[0m\x1b[3;1Hxyz\x1b[1G\x1b[2P\x1b[2@"),
            ("scroll-region", "1\r\n2\r\n3\r\n4\r\n5\x1b[2;4r\x1b[4;1H\nnew\x1b[r\x1b[5;1Hend"),
            ("reverse-index", "a\r\nb\r\nc\x1b[1;1H\x1bMtop"),
            ("alt-screen", "main text\x1b[?1049h\x1b[2J\x1b[Halt text\x1b[?1049l"),
            ("alt-screen-open", "main\x1b[?1049h\x1b[Halt \x1b[7mscreen\x1b[0m"),
            ("wrap", "0123456789abcdefghijklmnop\r\nxy"),
            ("wide-wrap", "123456789\u{4e2d}x\u{4e2d}"),
            ("tabs", "a\tb\tc\r\n\x1b[3g\x1bH\tx"),
            ("insert-mode", "abcdef\r\x1b[4hXY\x1b[4l"),
            ("dec-graphics", "\x1b(0lqk\x1b(B x"),
            ("bg-erase-below", "\x1b[48;5;22m\x1b[J\x1b[0m\x1b[2;3Hhi\x1b[3;1H\x1b[48;2;9;8;7m\x1b[2K\x1b[0m"),
        ];
        for (name, stream) in corpus {
            let (a, g) = both(12, 5, stream.as_bytes());
            assert_eq!(g.text(), a.text(), "{name}: text");
            assert_eq!(crate::term::rows_v7(&g), crate::term::rows_v7(&a), "{name}: protocol-7 rows");
            assert_eq!(g.lines, a.lines, "{name}: protocol-8 cells");
            assert_eq!(g.links, a.links, "{name}: links");
            assert_eq!(g.cursor, a.cursor, "{name}: cursor");
            assert_eq!((g.display_offset, g.history), (a.display_offset, a.history), "{name}: scrollback");
        }
    }

    /// Reviewed differences between the engines (also listed in docs/GHOSTTY.md).
    #[test]
    fn engines_differ_only_where_reviewed() {
        // 1. Overline (SGR 53) and blink (SGR 5) reach protocol 8 only with Ghostty;
        //    protocol 7 cannot carry them, so v7 rows still agree.
        let (a, g) = both(12, 2, b"\x1b[53mo\x1b[0m\x1b[5mk\x1b[0m");
        assert_eq!((g.lines[0][0].flags, g.lines[0][1].flags), (F8_OVERLINE, F8_BLINK));
        assert_eq!(a.lines[0][0].flags, 0);
        assert_eq!(crate::term::rows_v7(&g), crate::term::rows_v7(&a));

        // 2. Regional-indicator flags: Ghostty gives each indicator two columns, alacritty one.
        let (a, g) = both(12, 2, "\u{1f1e9}\u{1f1ea}!".as_bytes());
        assert_eq!((a.cursor, g.cursor), (Some((3, 0)), Some((5, 0))));
        assert_eq!(g.text(), a.text(), "same characters, different columns");

        // 3. Device attributes: Ghostty reports a VT220 with ANSI colour, alacritty a VT102.
        let mut alacritty = AlacrittyEmulator::new(&config(10, 2, false));
        let mut ghostty = emulator(10, 2);
        assert_eq!(feed_collect(&mut ghostty, b"\x1b[c").replies, vec![b"\x1b[?62;22c".to_vec()]);
        assert_eq!(feed_collect(&mut alacritty, b"\x1b[c").replies, vec![b"\x1b[?6c".to_vec()]);
        assert_eq!(feed_collect(&mut ghostty, b"\x1b[>c").replies, vec![b"\x1b[>1;10;0c".to_vec()]);

        // 4. OSC 9 (iTerm2 style) notifications: Ghostty reports them, alacritty only OSC 777.
        assert_eq!(feed_collect(&mut ghostty, b"\x1b]9;build done\x07").notifications, 1);
        assert_eq!(feed_collect(&mut alacritty, b"\x1b]9;build done\x07").notifications, 0);

        // 5. A full reset clears the progress state in Ghostty (an extra `progress(0)`).
        assert_eq!(feed_collect(&mut ghostty, b"\x1bc").progress, vec![0]);
        assert_eq!(feed_collect(&mut alacritty, b"\x1bc").progress, Vec::<u8>::new());

        // 6. SGR 21 is a double underline in Ghostty (ECMA-48); alacritty ignores it.
        let (a, g) = both(12, 2, b"\x1b[21mx\x1b[0m");
        assert_eq!(g.lines[0][0].flags, F_UNDERLINE as u32 | UL_DOUBLE << F8_UL_SHIFT);
        assert_eq!(a.lines[0][0].flags, 0);

        // 7. Erase display (ED 2) clears in place in Ghostty (xterm); alacritty
        //    first scrolls the screen into the scrollback.
        let (a, g) = both(12, 5, b"abc\r\ndef\x1b[2J");
        assert_eq!(g.text(), a.text());
        assert_eq!((a.history, g.history), (2, 0));
    }
}
