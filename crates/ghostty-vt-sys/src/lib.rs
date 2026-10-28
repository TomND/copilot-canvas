//! Raw FFI bindings to libghostty-vt, the terminal core of Ghostty, linked
//! statically from the pinned build of `cargo xtask build-ghostty`.
//!
//! The declarations in [`bindings`] are hand-written for the subset the
//! terminal host uses; the tests below verify them against the vendored
//! headers (`vendor/CHECKSUMS.sha256`) and the library's own ABI manifest.
//! Handles are not thread-safe: use each terminal from one thread.

#![allow(non_camel_case_types)]

mod bindings;

pub use bindings::*;

/// The pinned Ghostty commit this crate links.
pub const GHOSTTY_COMMIT: &str = env!("GHOSTTY_VT_COMMIT");
/// The Zig version the library is built with.
pub const ZIG_VERSION: &str = env!("GHOSTTY_VT_ZIG_VERSION");

/// Sized structs: zero-initialized with `size` set (`GHOSTTY_INIT_SIZED`).
macro_rules! sized {
    ($($ty:ident),*) => {$(
        impl $ty {
            pub fn sized() -> Self {
                // SAFETY: every field is an integer, bool, raw pointer, union of
                // those, or an `Option` of a function pointer; all-zero is valid.
                let mut value: Self = unsafe { std::mem::zeroed() };
                value.size = std::mem::size_of::<Self>();
                value
            }
        }
    )*};
}

sized!(GhosttyStyle, GhosttyGridRef, GhosttyRenderStateCursor, GhosttyClipboardWriteReply);

impl GhosttyString {
    /// The bytes, or empty for a NULL pointer.
    ///
    /// # Safety
    /// `ptr` must point to `len` readable bytes for the returned lifetime.
    pub unsafe fn as_bytes<'a>(self) -> &'a [u8] {
        if self.ptr.is_null() || self.len == 0 {
            &[]
        } else {
            // SAFETY: upheld by the caller.
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::ffi::CStr;
    use std::path::{Path, PathBuf};

    use serde_json::Value;
    use sha2::{Digest, Sha256};

    use super::*;

    fn crate_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn sha256(path: &Path) -> String {
        let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect()
    }

    fn files_below(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files_below(&path, out);
            } else {
                out.push(path);
            }
        }
    }

    #[test]
    fn vendored_headers_and_bindings_match_their_checksums() {
        let dir = crate_dir();
        let text = std::fs::read_to_string(dir.join("vendor/CHECKSUMS.sha256")).unwrap();
        assert!(text.contains(&format!("# ghostty-commit: {GHOSTTY_COMMIT}")), "pin line");
        assert!(text.contains(&format!("# zig-version: {ZIG_VERSION}")), "zig line");
        let mut listed = BTreeSet::new();
        for line in text.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
            let (hash, file) = line.split_once("  ").expect("`<sha256>  <path>` line");
            assert_eq!(sha256(&dir.join(file)), hash, "{file} changed; review it and run `cargo xtask vendor-ghostty --checksums-only`");
            listed.insert(file.to_owned());
        }
        assert!(listed.contains("src/bindings.rs") && listed.contains("vendor/LICENSE") && listed.contains("vendor/include/ghostty/vt.h"));
        let mut vendored = Vec::new();
        files_below(&dir.join("vendor/include"), &mut vendored);
        for path in vendored {
            let relative = path.strip_prefix(&dir).unwrap().to_string_lossy().replace('\\', "/");
            assert!(listed.contains(&relative), "{relative} is not in CHECKSUMS.sha256");
        }
    }

    fn manifest() -> Value {
        // SAFETY: returns a NUL-terminated static string.
        let json = unsafe { CStr::from_ptr(ghostty_type_json()) };
        serde_json::from_slice(json.to_bytes()).expect("ABI manifest is JSON")
    }

    fn number(value: &Value) -> i64 {
        value.as_i64().unwrap_or_else(|| panic!("number expected, got {value}"))
    }

    /// Size, alignment and every field offset of a `#[repr(C)]` struct.
    macro_rules! check_struct {
        ($types:expr, $ty:ident { $($field:ident),* $(,)? }) => {{
            let t = &$types[stringify!($ty)];
            assert_eq!(number(&t["size"]) as usize, std::mem::size_of::<$ty>(), "{} size", stringify!($ty));
            assert_eq!(number(&t["align"]) as usize, std::mem::align_of::<$ty>(), "{} align", stringify!($ty));
            let fields = t["fields"].as_object().expect("fields");
            let declared = [$(stringify!($field)),*];
            assert_eq!(fields.len(), declared.len(), "{} declares every field", stringify!($ty));
            $(
                assert_eq!(
                    number(&fields[stringify!($field)]["offset"]) as usize,
                    std::mem::offset_of!($ty, $field),
                    "{}.{}", stringify!($ty), stringify!($field)
                );
            )*
        }};
    }

    /// Size and alignment of a type (unions, aliases, opaque handles).
    macro_rules! check_size {
        ($types:expr, $($name:literal => $ty:ty),* $(,)?) => {$(
            let t = &$types[$name];
            assert_eq!(number(&t["size"]) as usize, std::mem::size_of::<$ty>(), "{} size", $name);
            assert_eq!(number(&t["align"]) as usize, std::mem::align_of::<$ty>(), "{} align", $name);
        )*};
    }

    /// Every listed constant has the manifest's value for `<prefix><NAME>`.
    macro_rules! check_enum {
        ($types:expr, $name:literal: [$($c:ident),* $(,)?]) => {{
            let t = &$types[$name];
            assert_eq!(number(&t["size"]), 4, "{} is int-sized", $name);
            let prefix = t["prefix"].as_str().expect("prefix");
            $(
                let key = stringify!($c).strip_prefix(prefix).unwrap_or_else(|| panic!("{} lacks prefix {prefix}", stringify!($c)));
                assert_eq!(t["values"][key].as_i64(), Some($c as i64), "{}", stringify!($c));
            )*
        }};
    }

    #[test]
    fn bindings_match_the_library_abi_manifest() {
        let manifest = manifest();
        let abi = &manifest["abi"];
        assert_eq!((abi["target"].as_str(), abi["os"].as_str(), abi["environment"].as_str()), (Some("x86_64"), Some("windows"), Some("msvc")));
        assert_eq!(number(&manifest["schema"]), 1, "manifest schema version");
        let types = &manifest["types"];

        check_struct!(types, GhosttyString { ptr, len });
        check_struct!(types, GhosttyColorRgb { r, g, b });
        check_struct!(types, GhosttyStyleColor { tag, value });
        check_struct!(types, GhosttyStyle {
            size, fg_color, bg_color, underline_color, bold, italic, faint, blink, inverse, invisible, strikethrough, overline, underline
        });
        check_struct!(types, GhosttyDeviceAttributesPrimary { conformance_level, features, num_features });
        check_struct!(types, GhosttyDeviceAttributesSecondary { device_type, firmware_version, rom_cartridge });
        check_struct!(types, GhosttyDeviceAttributesTertiary { unit_id });
        check_struct!(types, GhosttyDeviceAttributes { primary, secondary, tertiary });
        check_struct!(types, GhosttySizeReportSize { rows, columns, cell_width, cell_height });
        check_struct!(types, GhosttyPointCoordinate { x, y });
        check_struct!(types, GhosttyPoint { tag, value });
        check_struct!(types, GhosttyGridRef { size, node, x, y });
        check_struct!(types, GhosttyCellsView { ptr, len });
        check_struct!(types, GhosttyTerminalScrollViewport { tag, value });
        check_struct!(types, GhosttyTerminalScrollbar { total, offset, len });
        check_struct!(types, GhosttyClipboardContent { mime, data });
        check_struct!(types, GhosttyClipboardWriteReply { size, result, remember });
        check_struct!(types, GhosttyClipboardWrite { size, location, contents, contents_len, name, granted, can_remember, ctx, reply });
        check_struct!(types, GhosttyTerminalDesktopNotification { size, title, body });
        check_struct!(types, GhosttyTerminalProgressReport { size, state, progress });
        check_struct!(types, GhosttyTerminalModeConfig { mode, value });
        check_struct!(types, GhosttyRenderStateCursor {
            size, viewport_has_value, viewport_x, viewport_y, wide_tail, visible, blinking, password_input, visual_style
        });
        check_size!(types,
            "GhosttyStyleColorValue" => GhosttyStyleColorValue,
            "GhosttyPointValue" => GhosttyPointValue,
            "GhosttyTerminalScrollViewportValue" => GhosttyTerminalScrollViewportValue,
            "GhosttyColorPaletteIndex" => GhosttyColorPaletteIndex,
            "GhosttyMode" => GhosttyMode,
            "GhosttyMods" => GhosttyMods,
            "GhosttyKittyKeyFlags" => GhosttyKittyKeyFlags,
            "GhosttyCell" => GhosttyCell,
            "GhosttyRow" => GhosttyRow,
            "GhosttyTerminal" => GhosttyTerminal,
            "GhosttyRenderState" => GhosttyRenderState,
            "GhosttyRenderStateRowIterator" => GhosttyRenderStateRowIterator,
            "GhosttyRenderStateRowCells" => GhosttyRenderStateRowCells,
            "GhosttyKeyEncoder" => GhosttyKeyEncoder,
            "GhosttyKeyEvent" => GhosttyKeyEvent,
        );

        check_enum!(types, "GhosttyResult": [GHOSTTY_SUCCESS, GHOSTTY_OUT_OF_MEMORY, GHOSTTY_INVALID_VALUE, GHOSTTY_OUT_OF_SPACE, GHOSTTY_NO_VALUE]);
        check_enum!(types, "GhosttyStyleColorTag": [GHOSTTY_STYLE_COLOR_NONE, GHOSTTY_STYLE_COLOR_PALETTE, GHOSTTY_STYLE_COLOR_RGB]);
        check_enum!(types, "GhosttySgrUnderline": [
            GHOSTTY_SGR_UNDERLINE_NONE, GHOSTTY_SGR_UNDERLINE_SINGLE, GHOSTTY_SGR_UNDERLINE_DOUBLE,
            GHOSTTY_SGR_UNDERLINE_CURLY, GHOSTTY_SGR_UNDERLINE_DOTTED, GHOSTTY_SGR_UNDERLINE_DASHED,
        ]);
        check_enum!(types, "GhosttyPointTag": [GHOSTTY_POINT_TAG_ACTIVE, GHOSTTY_POINT_TAG_VIEWPORT, GHOSTTY_POINT_TAG_SCREEN, GHOSTTY_POINT_TAG_HISTORY]);
        check_enum!(types, "GhosttyCellContentTag": [
            GHOSTTY_CELL_CONTENT_CODEPOINT, GHOSTTY_CELL_CONTENT_CODEPOINT_GRAPHEME,
            GHOSTTY_CELL_CONTENT_BG_COLOR_PALETTE, GHOSTTY_CELL_CONTENT_BG_COLOR_RGB,
        ]);
        check_enum!(types, "GhosttyCellWide": [GHOSTTY_CELL_WIDE_NARROW, GHOSTTY_CELL_WIDE_WIDE, GHOSTTY_CELL_WIDE_SPACER_TAIL, GHOSTTY_CELL_WIDE_SPACER_HEAD]);
        check_enum!(types, "GhosttyCellData": [
            GHOSTTY_CELL_DATA_INVALID, GHOSTTY_CELL_DATA_CODEPOINT, GHOSTTY_CELL_DATA_CONTENT_TAG, GHOSTTY_CELL_DATA_WIDE,
            GHOSTTY_CELL_DATA_HAS_TEXT, GHOSTTY_CELL_DATA_HAS_STYLING, GHOSTTY_CELL_DATA_STYLE_ID, GHOSTTY_CELL_DATA_HAS_HYPERLINK,
            GHOSTTY_CELL_DATA_PROTECTED, GHOSTTY_CELL_DATA_SEMANTIC_CONTENT, GHOSTTY_CELL_DATA_COLOR_PALETTE, GHOSTTY_CELL_DATA_COLOR_RGB,
        ]);
        check_enum!(types, "GhosttyTerminalScrollViewportTag": [
            GHOSTTY_SCROLL_VIEWPORT_TOP, GHOSTTY_SCROLL_VIEWPORT_BOTTOM, GHOSTTY_SCROLL_VIEWPORT_DELTA,
            GHOSTTY_SCROLL_VIEWPORT_ROW, GHOSTTY_SCROLL_VIEWPORT_DELTA_PROMPT,
        ]);
        check_enum!(types, "GhosttyTerminalScreen": [GHOSTTY_TERMINAL_SCREEN_PRIMARY, GHOSTTY_TERMINAL_SCREEN_ALTERNATE]);
        check_enum!(types, "GhosttyClipboardLocation": [
            GHOSTTY_CLIPBOARD_LOCATION_STANDARD, GHOSTTY_CLIPBOARD_LOCATION_SELECTION, GHOSTTY_CLIPBOARD_LOCATION_PRIMARY,
        ]);
        check_enum!(types, "GhosttyClipboardWriteResult": [GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS, GHOSTTY_CLIPBOARD_WRITE_RESULT_DENIED]);
        check_enum!(types, "GhosttyTerminalProgressState": [
            GHOSTTY_TERMINAL_PROGRESS_STATE_REMOVE, GHOSTTY_TERMINAL_PROGRESS_STATE_SET, GHOSTTY_TERMINAL_PROGRESS_STATE_ERROR,
            GHOSTTY_TERMINAL_PROGRESS_STATE_INDETERMINATE, GHOSTTY_TERMINAL_PROGRESS_STATE_PAUSE,
        ]);
        check_enum!(types, "GhosttyTerminalOption": [
            GHOSTTY_TERMINAL_OPT_USERDATA, GHOSTTY_TERMINAL_OPT_WRITE_PTY, GHOSTTY_TERMINAL_OPT_BELL, GHOSTTY_TERMINAL_OPT_ENQUIRY,
            GHOSTTY_TERMINAL_OPT_XTVERSION, GHOSTTY_TERMINAL_OPT_TITLE_CHANGED, GHOSTTY_TERMINAL_OPT_SIZE,
            GHOSTTY_TERMINAL_OPT_COLOR_SCHEME, GHOSTTY_TERMINAL_OPT_DEVICE_ATTRIBUTES, GHOSTTY_TERMINAL_OPT_TITLE,
            GHOSTTY_TERMINAL_OPT_PWD, GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND, GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND,
            GHOSTTY_TERMINAL_OPT_COLOR_CURSOR, GHOSTTY_TERMINAL_OPT_COLOR_PALETTE, GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_STORAGE_LIMIT,
            GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_FILE, GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_TEMP_FILE,
            GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_SHARED_MEM, GHOSTTY_TERMINAL_OPT_APC_MAX_BYTES,
            GHOSTTY_TERMINAL_OPT_APC_MAX_BYTES_KITTY, GHOSTTY_TERMINAL_OPT_SELECTION, GHOSTTY_TERMINAL_OPT_DEFAULT_CURSOR_STYLE,
            GHOSTTY_TERMINAL_OPT_DEFAULT_CURSOR_BLINK, GHOSTTY_TERMINAL_OPT_GLYPH_PROTOCOL, GHOSTTY_TERMINAL_OPT_PWD_CHANGED,
            GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE, GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES, GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES,
            GHOSTTY_TERMINAL_OPT_DESKTOP_NOTIFICATION, GHOSTTY_TERMINAL_OPT_PROGRESS_REPORT,
            GHOSTTY_TERMINAL_OPT_CONTINUATION_MAX_BYTES, GHOSTTY_TERMINAL_OPT_TITLE_REPORT, GHOSTTY_TERMINAL_OPT_MODE_DEFAULT,
            GHOSTTY_TERMINAL_OPT_MODE, GHOSTTY_TERMINAL_OPT_UNKNOWN_SEQUENCE, GHOSTTY_TERMINAL_OPT_UNKNOWN_MAX_BYTES,
            GHOSTTY_TERMINAL_OPT_TERMINFO_NAME, GHOSTTY_TERMINAL_OPT_CLIPBOARD_READ, GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE_MAX_BYTES,
            GHOSTTY_TERMINAL_OPT_RESIZE_PULL_SCROLLBACK, GHOSTTY_TERMINAL_OPT_RENDER_HOLD, GHOSTTY_TERMINAL_OPT_SEMANTIC_PROMPT,
            GHOSTTY_TERMINAL_OPT_RESET, GHOSTTY_TERMINAL_OPT_XT_CHECKSUM_REPORT, GHOSTTY_TERMINAL_OPT_XT_CHECKSUM_EXTENSION,
            GHOSTTY_TERMINAL_OPT_PROGRAM_STATUS, GHOSTTY_TERMINAL_OPT_RESIZE_WINDOW,
        ]);
        check_enum!(types, "GhosttyTerminalData": [
            GHOSTTY_TERMINAL_DATA_INVALID, GHOSTTY_TERMINAL_DATA_COLS, GHOSTTY_TERMINAL_DATA_ROWS, GHOSTTY_TERMINAL_DATA_CURSOR_X,
            GHOSTTY_TERMINAL_DATA_CURSOR_Y, GHOSTTY_TERMINAL_DATA_CURSOR_PENDING_WRAP, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN,
            GHOSTTY_TERMINAL_DATA_CURSOR_VISIBLE, GHOSTTY_TERMINAL_DATA_KITTY_KEYBOARD_FLAGS, GHOSTTY_TERMINAL_DATA_SCROLLBAR,
            GHOSTTY_TERMINAL_DATA_CURSOR_STYLE, GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING, GHOSTTY_TERMINAL_DATA_TITLE,
            GHOSTTY_TERMINAL_DATA_PWD, GHOSTTY_TERMINAL_DATA_TOTAL_ROWS, GHOSTTY_TERMINAL_DATA_SCROLLBACK_ROWS,
            GHOSTTY_TERMINAL_DATA_WIDTH_PX, GHOSTTY_TERMINAL_DATA_HEIGHT_PX, GHOSTTY_TERMINAL_DATA_COLOR_FOREGROUND,
            GHOSTTY_TERMINAL_DATA_COLOR_BACKGROUND, GHOSTTY_TERMINAL_DATA_COLOR_CURSOR, GHOSTTY_TERMINAL_DATA_COLOR_PALETTE,
            GHOSTTY_TERMINAL_DATA_COLOR_FOREGROUND_DEFAULT, GHOSTTY_TERMINAL_DATA_COLOR_BACKGROUND_DEFAULT,
            GHOSTTY_TERMINAL_DATA_COLOR_CURSOR_DEFAULT, GHOSTTY_TERMINAL_DATA_COLOR_PALETTE_DEFAULT,
            GHOSTTY_TERMINAL_DATA_KITTY_IMAGE_STORAGE_LIMIT, GHOSTTY_TERMINAL_DATA_KITTY_IMAGE_MEDIUM_FILE,
            GHOSTTY_TERMINAL_DATA_KITTY_IMAGE_MEDIUM_TEMP_FILE, GHOSTTY_TERMINAL_DATA_KITTY_IMAGE_MEDIUM_SHARED_MEM,
            GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS, GHOSTTY_TERMINAL_DATA_SELECTION, GHOSTTY_TERMINAL_DATA_VIEWPORT_ACTIVE,
            GHOSTTY_TERMINAL_DATA_VT_PROCESSING_ERROR, GHOSTTY_TERMINAL_DATA_SCROLLBACK_MAX_BYTES,
            GHOSTTY_TERMINAL_DATA_SCROLLBACK_MAX_LINES, GHOSTTY_TERMINAL_DATA_CONTINUATION_MAX_BYTES, GHOSTTY_TERMINAL_DATA_MODE,
            GHOSTTY_TERMINAL_DATA_VT_GROUND, GHOSTTY_TERMINAL_DATA_CURSOR_AT_PROMPT, GHOSTTY_TERMINAL_DATA_CLIPBOARD_WRITE_MAX_BYTES,
            GHOSTTY_TERMINAL_DATA_MOUSE_SHAPE, GHOSTTY_TERMINAL_DATA_MEMORY_USAGE,
        ]);
        check_enum!(types, "GhosttyRenderStateDirty": [GHOSTTY_RENDER_STATE_DIRTY_FALSE, GHOSTTY_RENDER_STATE_DIRTY_PARTIAL, GHOSTTY_RENDER_STATE_DIRTY_FULL]);
        check_enum!(types, "GhosttyRenderStateCursorVisualStyle": [
            GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BAR, GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK,
            GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE, GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW,
        ]);
        check_enum!(types, "GhosttyRenderStateData": [
            GHOSTTY_RENDER_STATE_DATA_INVALID, GHOSTTY_RENDER_STATE_DATA_COLS, GHOSTTY_RENDER_STATE_DATA_ROWS,
            GHOSTTY_RENDER_STATE_DATA_DIRTY, GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR, GHOSTTY_RENDER_STATE_DATA_CURSOR,
            GHOSTTY_RENDER_STATE_DATA_IMAGE_ITERATOR, GHOSTTY_RENDER_STATE_DATA_IMAGE_PLACEMENTS_GENERATION,
        ]);
        check_enum!(types, "GhosttyImageProtocol": [GHOSTTY_IMAGE_PROTOCOL_KITTY]);
        check_enum!(types, "GhosttyRenderStateImageLayer": [
            GHOSTTY_RENDER_STATE_IMAGE_LAYER_BELOW_BACKGROUND, GHOSTTY_RENDER_STATE_IMAGE_LAYER_BELOW_TEXT, GHOSTTY_RENDER_STATE_IMAGE_LAYER_ABOVE_TEXT,
        ]);
        check_enum!(types, "GhosttyRenderStateImageData": [
            GHOSTTY_RENDER_STATE_IMAGE_DATA_INVALID, GHOSTTY_RENDER_STATE_IMAGE_DATA_PROTOCOL, GHOSTTY_RENDER_STATE_IMAGE_DATA_IMAGE_ID,
            GHOSTTY_RENDER_STATE_IMAGE_DATA_IMAGE_GENERATION, GHOSTTY_RENDER_STATE_IMAGE_DATA_LAYER, GHOSTTY_RENDER_STATE_IMAGE_DATA_Z,
            GHOSTTY_RENDER_STATE_IMAGE_DATA_VIEWPORT_COL, GHOSTTY_RENDER_STATE_IMAGE_DATA_VIEWPORT_ROW, GHOSTTY_RENDER_STATE_IMAGE_DATA_GRID_COLS,
            GHOSTTY_RENDER_STATE_IMAGE_DATA_GRID_ROWS, GHOSTTY_RENDER_STATE_IMAGE_DATA_CELL_OFFSET_X, GHOSTTY_RENDER_STATE_IMAGE_DATA_CELL_OFFSET_Y,
            GHOSTTY_RENDER_STATE_IMAGE_DATA_PIXEL_WIDTH, GHOSTTY_RENDER_STATE_IMAGE_DATA_PIXEL_HEIGHT, GHOSTTY_RENDER_STATE_IMAGE_DATA_SOURCE_X,
            GHOSTTY_RENDER_STATE_IMAGE_DATA_SOURCE_Y, GHOSTTY_RENDER_STATE_IMAGE_DATA_SOURCE_WIDTH, GHOSTTY_RENDER_STATE_IMAGE_DATA_SOURCE_HEIGHT,
        ]);
        check_enum!(types, "GhosttyKittyImageFormat": [
            GHOSTTY_KITTY_IMAGE_FORMAT_RGB, GHOSTTY_KITTY_IMAGE_FORMAT_RGBA, GHOSTTY_KITTY_IMAGE_FORMAT_PNG,
            GHOSTTY_KITTY_IMAGE_FORMAT_GRAY_ALPHA, GHOSTTY_KITTY_IMAGE_FORMAT_GRAY,
        ]);
        check_enum!(types, "GhosttyKittyGraphicsImageData": [
            GHOSTTY_KITTY_IMAGE_DATA_INVALID, GHOSTTY_KITTY_IMAGE_DATA_ID, GHOSTTY_KITTY_IMAGE_DATA_NUMBER, GHOSTTY_KITTY_IMAGE_DATA_WIDTH,
            GHOSTTY_KITTY_IMAGE_DATA_HEIGHT, GHOSTTY_KITTY_IMAGE_DATA_FORMAT, GHOSTTY_KITTY_IMAGE_DATA_COMPRESSION,
            GHOSTTY_KITTY_IMAGE_DATA_DATA_PTR, GHOSTTY_KITTY_IMAGE_DATA_DATA_LEN, GHOSTTY_KITTY_IMAGE_DATA_GENERATION,
        ]);
        check_enum!(types, "GhosttySysOption": [GHOSTTY_SYS_OPT_USERDATA, GHOSTTY_SYS_OPT_DECODE_PNG, GHOSTTY_SYS_OPT_LOG]);
        check_struct!(types, GhosttySysImage { width, height, data, data_len });
        check_enum!(types, "GhosttyRenderStateRowData": [
            GHOSTTY_RENDER_STATE_ROW_DATA_INVALID, GHOSTTY_RENDER_STATE_ROW_DATA_DIRTY, GHOSTTY_RENDER_STATE_ROW_DATA_RAW,
            GHOSTTY_RENDER_STATE_ROW_DATA_CELLS, GHOSTTY_RENDER_STATE_ROW_DATA_CELLS_RAW, GHOSTTY_RENDER_STATE_ROW_DATA_VIEWPORT_Y,
        ]);
        check_enum!(types, "GhosttyRenderStateRowCellsData": [
            GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_INVALID, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW,
            GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN,
            GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF,
        ]);
        check_enum!(types, "GhosttyKeyAction": [GHOSTTY_KEY_ACTION_RELEASE, GHOSTTY_KEY_ACTION_PRESS, GHOSTTY_KEY_ACTION_REPEAT]);
        check_enum!(types, "GhosttyKey": [
            GHOSTTY_KEY_UNIDENTIFIED, GHOSTTY_KEY_BACKQUOTE, GHOSTTY_KEY_BACKSLASH, GHOSTTY_KEY_BRACKET_LEFT,
            GHOSTTY_KEY_BRACKET_RIGHT, GHOSTTY_KEY_COMMA, GHOSTTY_KEY_DIGIT_0, GHOSTTY_KEY_DIGIT_9, GHOSTTY_KEY_EQUAL,
            GHOSTTY_KEY_A, GHOSTTY_KEY_Z, GHOSTTY_KEY_MINUS, GHOSTTY_KEY_PERIOD, GHOSTTY_KEY_QUOTE, GHOSTTY_KEY_SEMICOLON,
            GHOSTTY_KEY_SLASH, GHOSTTY_KEY_BACKSPACE, GHOSTTY_KEY_ENTER, GHOSTTY_KEY_SPACE, GHOSTTY_KEY_TAB,
            GHOSTTY_KEY_DELETE, GHOSTTY_KEY_END, GHOSTTY_KEY_HOME, GHOSTTY_KEY_INSERT, GHOSTTY_KEY_PAGE_DOWN,
            GHOSTTY_KEY_PAGE_UP, GHOSTTY_KEY_ARROW_DOWN, GHOSTTY_KEY_ARROW_LEFT, GHOSTTY_KEY_ARROW_RIGHT,
            GHOSTTY_KEY_ARROW_UP, GHOSTTY_KEY_ESCAPE, GHOSTTY_KEY_F1, GHOSTTY_KEY_F12,
        ]);
        // The letters and digits in between are contiguous.
        for (first, last, count) in [("A", "Z", 26), ("DIGIT_0", "DIGIT_9", 10), ("F1", "F12", 12)] {
            let values = &types["GhosttyKey"]["values"];
            assert_eq!(number(&values[last]) - number(&values[first]) + 1, count, "{first}..{last}");
        }
        check_enum!(types, "GhosttyKeyEncoderOption": [
            GHOSTTY_KEY_ENCODER_OPT_CURSOR_KEY_APPLICATION, GHOSTTY_KEY_ENCODER_OPT_KEYPAD_KEY_APPLICATION,
            GHOSTTY_KEY_ENCODER_OPT_IGNORE_KEYPAD_WITH_NUMLOCK, GHOSTTY_KEY_ENCODER_OPT_ALT_ESC_PREFIX,
            GHOSTTY_KEY_ENCODER_OPT_MODIFY_OTHER_KEYS_STATE_2, GHOSTTY_KEY_ENCODER_OPT_KITTY_FLAGS,
            GHOSTTY_KEY_ENCODER_OPT_MACOS_OPTION_AS_ALT, GHOSTTY_KEY_ENCODER_OPT_BACKARROW_KEY_MODE,
        ]);
        check_enum!(types, "GhosttyFocusEvent": [GHOSTTY_FOCUS_GAINED, GHOSTTY_FOCUS_LOST]);
        check_enum!(types, "GhosttyBuildInfo": [GHOSTTY_BUILD_INFO_SIMD, GHOSTTY_BUILD_INFO_KITTY_GRAPHICS, GHOSTTY_BUILD_INFO_VERSION_STRING]);
    }

    /// Integer value of a `#define NAME <expr>` in the vendored headers.
    fn header_define(name: &str) -> i64 {
        let mut headers = Vec::new();
        files_below(&crate_dir().join("vendor/include"), &mut headers);
        for header in headers {
            let text = std::fs::read_to_string(&header).unwrap();
            for line in text.lines() {
                let mut words = line.split_whitespace();
                if words.next() != Some("#define") || words.next() != Some(name) {
                    continue;
                }
                let expr: String = words.collect::<Vec<_>>().join(" ");
                let expr = expr.split("/*").next().unwrap().trim();
                let expr = expr.trim_start_matches('(').trim_end_matches(')').trim();
                if let Some(args) = expr.strip_prefix("ghostty_mode_new(") {
                    let (value, ansi) = args.trim_end_matches(')').split_once(',').unwrap();
                    return ghostty_mode_new(value.trim().parse().unwrap(), ansi.trim() == "true") as i64;
                }
                if let Some((one, shift)) = expr.split_once("<<") {
                    return one.trim().parse::<i64>().unwrap() << shift.trim().parse::<i64>().unwrap();
                }
                return expr.parse().unwrap_or_else(|_| panic!("cannot evaluate {name} = {expr}"));
            }
        }
        panic!("#define {name} not found in the vendored headers");
    }

    macro_rules! check_defines {
        ($($c:ident),* $(,)?) => {$(
            assert_eq!(header_define(stringify!($c)), $c as i64, "{}", stringify!($c));
        )*};
    }

    #[test]
    fn macro_constants_match_the_vendored_headers() {
        check_defines!(
            GHOSTTY_MODS_SHIFT, GHOSTTY_MODS_CTRL, GHOSTTY_MODS_ALT, GHOSTTY_MODS_SUPER,
            GHOSTTY_KITTY_KEY_DISABLED, GHOSTTY_KITTY_KEY_DISAMBIGUATE, GHOSTTY_KITTY_KEY_REPORT_EVENTS,
            GHOSTTY_KITTY_KEY_REPORT_ALTERNATES, GHOSTTY_KITTY_KEY_REPORT_ALL, GHOSTTY_KITTY_KEY_REPORT_ASSOCIATED,
            GHOSTTY_DA_CONFORMANCE_VT220, GHOSTTY_DA_FEATURE_ANSI_COLOR, GHOSTTY_DA_DEVICE_TYPE_VT220,
            GHOSTTY_MODE_INSERT, GHOSTTY_MODE_DECCKM, GHOSTTY_MODE_WRAPAROUND, GHOSTTY_MODE_X10_MOUSE,
            GHOSTTY_MODE_CURSOR_BLINKING, GHOSTTY_MODE_CURSOR_VISIBLE, GHOSTTY_MODE_KEYPAD_KEYS,
            GHOSTTY_MODE_NORMAL_MOUSE, GHOSTTY_MODE_BUTTON_MOUSE, GHOSTTY_MODE_ANY_MOUSE, GHOSTTY_MODE_FOCUS_EVENT,
            GHOSTTY_MODE_UTF8_MOUSE, GHOSTTY_MODE_SGR_MOUSE, GHOSTTY_MODE_ALT_ESC_PREFIX, GHOSTTY_MODE_ALT_SCREEN,
            GHOSTTY_MODE_ALT_SCREEN_SAVE, GHOSTTY_MODE_BRACKETED_PASTE, GHOSTTY_MODE_SYNC_OUTPUT,
            GHOSTTY_MODE_GRAPHEME_CLUSTER,
        );
    }

    #[test]
    fn a_terminal_parses_output_and_frees_cleanly() {
        // SAFETY: the handle is created, used and freed on this thread.
        unsafe {
            let mut terminal: GhosttyTerminal = std::ptr::null_mut();
            assert_eq!(ghostty_terminal_new(std::ptr::null(), &mut terminal, 20, 5), GHOSTTY_SUCCESS);
            let bytes = b"\x1b]2;hello\x07ab\r\nc";
            ghostty_terminal_vt_write(terminal, bytes.as_ptr(), bytes.len());
            let (mut x, mut y) = (0u16, 0u16);
            assert_eq!(ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_CURSOR_X, (&raw mut x).cast()), GHOSTTY_SUCCESS);
            assert_eq!(ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_CURSOR_Y, (&raw mut y).cast()), GHOSTTY_SUCCESS);
            assert_eq!((x, y), (1, 1));
            let mut title = GhosttyString { ptr: std::ptr::null(), len: 0 };
            assert_eq!(ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_TITLE, (&raw mut title).cast()), GHOSTTY_SUCCESS);
            assert_eq!(title.as_bytes(), b"hello");
            let mut simd = false;
            assert_eq!(ghostty_build_info(GHOSTTY_BUILD_INFO_SIMD, (&raw mut simd).cast()), GHOSTTY_SUCCESS);
            ghostty_terminal_free(terminal);
        }
    }
}
