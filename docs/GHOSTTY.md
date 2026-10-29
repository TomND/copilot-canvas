# libghostty-vt engine (Phase 2)

Copilot Canvas can run every hosted terminal on Ghostty's VT core
(libghostty-vt) instead of alacritty_terminal. The engine sits behind the
`Emulator` trait (PLAN.md §4 D1) and is enabled per build with the `ghostty`
Cargo feature and per service with `COPILOT_CANVAS_ENGINE=ghostty`.

| Piece | Where |
|---|---|
| Build step | `crates/xtask` (`cargo xtask build-ghostty`, `cargo xtask vendor-ghostty`) |
| FFI crate | `crates/ghostty-vt-sys` (vendored headers, hand-written bindings, checksums, link) |
| Engine | `crates/canvas-host/src/emulator/ghostty.rs` (`GhosttyEmulator`) |
| Features | root `ghostty = ["canvas-host/ghostty"]`, canvas-host `ghostty = ["dep:ghostty-vt-sys"]` |

## Pin

| | |
|---|---|
| Ghostty commit | `521cf2c5d30bd48028ca61546c64e7913764073d` (2026-10-21, "Update VOUCHED list (#14798)"), the commit libghostty-rs master `9ede35b8` pins |
| Zig | 0.17.0 (`C:\tools\zig-x86_64-windows-0.17.0\zig.exe`) |
| Zig flags | `-Demit-lib-vt=true -Doptimize=ReleaseFast -Dcpu=baseline` |
| `ghostty-vt-static.lib` sha256 | `a04be69ba8e2500c2a25369da5656b12a9219db292afd2abd3f8efa41c121baa` (reproducible: a `--force` rebuild gives the same hash) |

The pin lives in one place: the header of
`crates/ghostty-vt-sys/vendor/CHECKSUMS.sha256` (`# ghostty-commit:` and
`# zig-version:`). The xtask, the sys crate's build script and its tests all
read it.

## Build

```powershell
cargo xtask build-ghostty            # once per pin; skipped when up to date
cargo build --release --features ghostty
$env:COPILOT_CANVAS_ENGINE = 'ghostty'   # service-wide engine for new terminals
```

`cargo xtask build-ghostty [--force]`:

1. reads the pin;
2. checks out the source into `target/ghostty/src-<12-char commit>` (`git clone
   --filter=blob:none` with `core.autocrlf=false` so files are byte-exact, then
   `checkout <commit>` and a `rev-parse` check; if git fails it downloads
   `https://github.com/ghostty-org/ghostty/archive/<commit>.tar.gz` with
   `curl.exe` and `tar.exe`);
3. finds Zig (`ZIG`, then `C:\tools\zig-x86_64-windows-0.17.0\zig.exe`, then
   `zig` on PATH) and requires `zig version` to equal the pin;
4. runs `zig build` with the flags above into `target/ghostty/<commit>/`
   (`--cache-dir target/ghostty/zig-cache`);
5. verifies `lib/ghostty-vt-static.lib` and `include/ghostty/vt.h`, and that
   every installed header matches `vendor/CHECKSUMS.sha256` (the bindings were
   checked against those headers);
6. writes `target/ghostty/<commit>/BUILD-INFO.txt` (commit, Zig version, flags,
   library sha256, time). It is written last, so an interrupted build is
   rebuilt; a later run with the same pin, flags and library hash is a no-op.

Timing on the Ryzen 7600X: 140 s for the first build (including Zig's
dependency fetch into its global cache), 17 s for a forced rebuild.

What the Windows build needed: nothing beyond Zig. Zig locates the MSVC
libraries and the Windows SDK itself, so no `vcvars` environment is required,
and Ghostty's build defaults Windows targets to the MSVC ABI, which links
into Rust's `x86_64-pc-windows-msvc` binaries. SIMD stays enabled (simdutf and
highway resolve inside the archive; `-Dsimd=false` was not needed).
`-Dcpu=baseline` keeps the library runnable on any x86-64 machine; the SIMD
paths are selected at run time. The DLL and import library are built too but
not used.

## Linking

`crates/ghostty-vt-sys/build.rs` links the archive statically:

- library directory: `GHOSTTY_VT_LIB_DIR` if set, else
  `<workspace>/target/ghostty/<commit>/lib` (whose `BUILD-INFO.txt` must name
  the pinned commit);
- `cargo:rustc-link-lib=static:+verbatim=ghostty-vt-static.lib` (`+verbatim`
  so MSVC does not resolve `ghostty-vt` to the DLL import library);
- system libraries: `ntdll` and `kernel32` (the Zig standard library inside
  the archive); nothing else was unresolved;
- target must be `*-windows-msvc`.

If the library is missing the build fails with
`libghostty-vt 521cf2c5d30b not found: ... build it with cargo xtask
build-ghostty (needs git and Zig 0.17.0)`. The build script deliberately does
not run Zig itself: the build fetches sources and takes minutes, which should
be an explicit step rather than a side effect of `cargo check`. Because
`ghostty-vt-sys` is a workspace member, `cargo test --workspace` and
`cargo clippy --workspace` also need the library (run the xtask once, or set
`GHOSTTY_VT_LIB_DIR`). Builds without the `ghostty` feature never link it.

## Bindings and checksums

`ghostty-vt-sys/src/bindings.rs` is hand-written for the subset the engine
uses (terminal, render state, cells, grid refs, key encoder, paste, focus,
build info), with the C names. Three tests keep it honest:

- `vendored_headers_and_bindings_match_their_checksums`: every file listed in
  `vendor/CHECKSUMS.sha256` (35 headers, `vendor/LICENSE`, `src/bindings.rs`)
  has its recorded sha256, and no vendored header is unlisted;
- `bindings_match_the_library_abi_manifest`: size, alignment and every field
  offset of each `#[repr(C)]` struct, and the value of every enum constant,
  equal the library's own ABI manifest (`ghostty_type_json()`, schema 1,
  target x86_64-windows-msvc);
- `macro_constants_match_the_vendored_headers`: the `#define` constants
  (modifier bits, kitty flags, DA codes, `GHOSTTY_MODE_*`) equal the headers.

`vendor/` and `src/bindings.rs` are marked `-text` in
`crates/ghostty-vt-sys/.gitattributes` so checkouts keep the exact bytes.
Ghostty is MIT licensed; its license is vendored as `vendor/LICENSE`.

### Update policy

Engine updates are one reviewed change together with the golden corpus:

1. change `# ghostty-commit:` (and `# zig-version:` if Ghostty requires it);
2. `cargo xtask build-ghostty` (it refuses to finish while the new headers
   differ from the vendored ones), then `cargo xtask vendor-ghostty`;
3. review the header diff, update `src/bindings.rs`, then
   `cargo xtask vendor-ghostty --checksums-only`;
4. `cargo test -p ghostty-vt-sys` (ABI manifest), `cargo test -p canvas-host
   --features ghostty` (engine, key table, cross-engine corpus) and the golden
   corpus; record any new difference below.

## Engine design

- **Ownership.** `GhosttyEmulator` owns a terminal, a render state, a row
  iterator, a row-cells handle, a key encoder and a key event; `Drop` frees
  them all. It is `!Send` and lives on its terminal's emulator thread.
- **Events.** Callbacks (write-pty, bell, title, reset, size, device
  attributes, clipboard write, desktop notification, progress, render hold)
  only append to a heap `Shared` reached through the userdata pointer; `feed`
  delivers them after `vt_write` returns, so nothing re-enters the terminal.
  The title is read once after the write (`DATA_TITLE`; empty means `None`).
- **Clipboard.** OSC 52 / OSC 1337 / OSC 5522 writes become `Events::clipboard`
  (the `text/plain` representation). No clipboard-read callback is installed,
  so OSC 52 reads get no reply.
- **Replies.** Cursor reports, mode reports and colour queries are Ghostty's.
  Default colours are the alacritty engine's palette (Campbell + xterm cube),
  so OSC 4/10/11 answers are identical. Size queries report 8×16 cells like
  alacritty. DA1/DA2 are answered as Ghostty does (see differences).
- **Snapshot.** Rows come from the render state: raw cells in bulk, per-cell
  fields via `ghostty_cell_get_multi`, styles (cached per style id) and
  grapheme code points through the row-cells handle, OSC 8 targets through a
  grid ref per linked cell. Colours map exactly like alacritty.rs
  (`COLOR_DEFAULT`, palette index + 1, `COLOR_RGB | rgb`), faint is `F_DIM`,
  spacer cells are skipped, and trailing blanks are trimmed with the same rule.
- **Synchronized output (DEC 2026).** The render-hold callback captures the
  render state when a hold begins; snapshots during the hold show that frame
  (alacritty shows the pre-sync screen the same way).
  `sync_deadline` is hold start + 150 ms (alacritty's limit); `end_sync`
  resets mode 2026. A resize ends a hold; scrolling shows live content.
- **Keys.** Ghostty's key encoder, configured from the terminal
  (`setopt_from_terminal`): characters carry their text, a US-layout physical
  key and the unshifted code point; Shift counts as consumed for text keys.
  With kitty flags on, Ghostty's output is used as is. Without them the bytes
  go through ConPTY's input parser, so the few xterm extensions Ghostty adds
  in legacy mode (fixterms `CSI <code>;<mods> u`, modifyOtherKeys
  `CSI 27;<mods>;<code> ~`, kitty's `CSI 13;<mods> ~` for a modified F3) fall
  back to the `keys.rs` bytes. Control characters and F13+ also use `keys.rs`.
- **Mouse.** `encode_wheel`/`encode_click` call `crate::keys` with the engine's
  modes: the wheel policy (Shift scrolls history, PageUp/PageDown on the
  alternate screen) is the host's, Ghostty's mouse encoder needs pixel
  geometry, and its X10 (DECSET 9) reports differ from the keys.rs format.
- **Paste.** Both engines follow `keys::paste_lines`: bracket markers are
  stripped (also ones that removing another forms), CR LF, CR and LF are line
  breaks and trailing ones are dropped. With bracketed paste on, the lines are
  joined with CR and `ghostty_paste_encode` frames them (and blanks unsafe
  control bytes); without it each line is encoded alone and the lines are
  joined with Shift+Enter as the program reads it (`CSI 13;2u` with kitty
  flags, else ESC CR from the legacy fallback), never with a bare CR.
- **Soft wraps.** A row whose `GHOSTTY_ROW_DATA_WRAP` is set (read from the
  render state's raw row, `ghostty_row_get`) ends with the `F8_SOFTWRAP`
  marker run; a `SPACER_HEAD` in the last column (a wide character that moved
  on) puts the marker one column earlier. alacritty reads `WRAPLINE` and
  `LEADING_WIDE_CHAR_SPACER` on the row's last cell.
- **Other settings.** Scrollback limit = `Config::scrollback` lines;
  `RESIZE_PULL_SCROLLBACK = false` (ConPTY keeps no scrollback, Ghostty's
  documented setting for it); terminfo name `xterm-256color`; grapheme
  clustering (mode 2027) stays off unless the program enables it, which keeps
  widths aligned with ConPTY's.
- **Robustness.** `vt_write` never fails on malformed input; a randomized
  escape-heavy fuzz test with resizes, oversized parameters, invalid UTF-8 and
  unterminated strings, plus 50 create/drop cycles, runs in the unit tests.
- **Fallback.** `emulator::create` falls back to alacritty if
  `GhosttyEmulator::new` fails. The service log records the engine at launch:
  `launched <id> engine <name> pid <pid>` (the pid stays last for
  `selftest copilot`).

## Verification (2026-10-27)

| Step | Result |
|---|---|
| `cargo xtask build-ghostty` | built (140 s cold, 17 s `--force`); then "up to date" |
| `cargo test -p ghostty-vt-sys` | 4 passed |
| `cargo test -p canvas-host --features ghostty` | 40 passed (16 existing + 24 Ghostty) |
| `cargo test --workspace` | all passed (canvas-agents 45 + 2 ignored, common 8, core 26, host 16, model 19, termview 4, tui 2, ghostty-vt-sys 4, xtask 2; root binary 17 ignored probes) |
| `cargo clippy --workspace --all-targets [--features ghostty]` | clean |
| `cargo build --release --features ghostty` | 10.4 MB `copilot-canvas.exe` |
| `COPILOT_CANVAS_ENGINE=ghostty copilot-canvas selftest` | `PASS in 27.8s (first paint incl. service start: 720 ms)`; service log: every terminal `engine ghostty` |

### `bench --terminals 40 --seconds 8`

| Engine | keystroke p50 / p95 / p99 (ms) | service CPU % of one core | private KB per terminal | working set |
|---|---|---|---|---|
| alacritty | 1.15 / 4.72 / 5.18 | 11.7 | 5330 | 142 MB |
| ghostty | 1.09 / 4.62 / 10.25 | 10.7 | 2256 | 53 MB |

Two extra pairs to gauge noise (p99 is roughly the 3rd-worst of 296 samples):

| Engine | p50 / p95 / p99 (ms) | CPU % | private KB per terminal |
|---|---|---|---|
| alacritty | 1.14 / 4.72 / 9.22 | 8.0 | 5362 |
| ghostty | 1.11 / 4.66 / 5.49 | 12.3 | 2259 |
| alacritty | 1.16 / 4.66 / 4.90 | 9.2 | 5327 |
| ghostty | 1.11 / 4.68 / 5.27 | 11.1 | 2256 |

Reading: memory per terminal is 58 % lower and working set 63 % lower; p50
and p95 are equal or slightly better; p99 and CPU vary more between runs
than between engines (CPU 8.0–11.7 % alacritty, 10.7–12.3 % Ghostty). The
engine itself is faster in release micro-benchmarks: about 0.73 µs versus
1.5 µs per bench-style output line (100×30), 356 versus 112 MB/s for bulk
coloured output, and equal snapshot cost (about 50 µs for 100×30, 105 versus
165 µs for a dense 200×50 screen). The PLAN exit criterion
(`bench --terminals 100` CPU at or below alacritty) still needs a longer,
repeated measurement before the default changes.

## Known differences (reviewed)

Covered by `engines_agree_on_the_common_corpus` (SGR 16/256/truecolour incl.
colon form, bold/italic/inverse/strike/dim/hidden, all underline styles and
underline colours, CJK, emoji incl. skin tones and VS16, ZWJ sequences such as
woman technologist (U+1F469 U+200D U+1F4BB) and a family sequence, combining
marks, Hangul jamo, OSC 8 with ids, cursor movement,
erase with colours, scroll regions, reverse index, alternate screen enter and
exit, line wrap incl. wide-character wrap, tab stops, insert mode, DEC line
drawing: identical text, protocol-7 rows, protocol-8 cells, links, cursor and
scrollback) and `engines_differ_only_where_reviewed`:

1. **Overline and blink** (SGR 53, SGR 5) reach protocol 8 (`F8_OVERLINE`,
   `F8_BLINK`) only with Ghostty; protocol-7 rows are unchanged.
2. **Regional-indicator flags** (U+1F1E9 U+1F1EA): Ghostty gives each
   indicator two columns, alacritty one. Same characters, different columns.
3. **Device attributes:** DA1 `CSI ? 62 ; 22 c` (VT220 + ANSI colour) and DA2
   `CSI > 1 ; 10 ; 0 c`, as Ghostty answers; alacritty sends `CSI ? 6 c` and
   `CSI > 0 ; <version> ; 1 c`.
4. **OSC 9 notifications** (iTerm2 `OSC 9 ; text`) count as notifications with
   Ghostty; alacritty only reports OSC 777. A malformed `OSC 9;4` progress
   report is ignored by both.
5. **Full reset (RIS)** also clears the progress state (an extra
   `progress(0)` event).
6. **SGR 21** is a double underline (ECMA-48); alacritty ignores it.
7. **Erase display (ED 2)** clears in place (xterm); alacritty first scrolls
   the screen into the scrollback, so `history` differs afterwards.

Input (`legacy_key_table_matches_keys_rs_with_reviewed_differences`,
`kitty_key_table_follows_the_protocol`): every key the TUI forwards was
compared with keys.rs in normal, application-cursor and application-keypad
modes.

8. **Legacy mode:** identical to keys.rs except Ctrl+`/` → `0x1f` and Ctrl+`~`
   → `0x1e` (xterm's C0 bytes; keys.rs sends the bare character).
9. **Kitty mode** follows the kitty protocol: modified text keys are CSI u
   (Ctrl+C → `CSI 99;5u`, Alt+x → `CSI 120;3u`), Alt/Ctrl+Enter, Shift+Tab /
   BackTab (`CSI 9;2u`), modified Backspace and Esc are CSI u, and F1–F4 use
   `CSI P/Q/13~/S`. keys.rs only special-cased Shift/Ctrl+Enter and Esc
   (those two are unchanged). Report-all-keys mode is honoured.
10. **Paste:** none any more (`pastes_encode_like_keys_rs_in_every_mode`):
    keys.rs blanks the same control bytes as Ghostty's encoder (NUL,
    Ctrl+C/D/E/O/Q/R/S/U/V/W/Z, BS, ESC, FS, DEL) and both join lines the
    same way in every mode.
11. **X10 mouse (DECSET 9)** turns on `MODE_MOUSE`; alacritty ignores the mode.
12. **`Config::kitty_keyboard = false`** has no effect: Ghostty always answers
    kitty keyboard queries (the service always sets it to true).

## Limitations and follow-ups

- While a synchronized update is held, OSC 8 targets of the held frame are
  read from the live terminal (link positions can lag for at most 150 ms).
- The service log names the configured engine; the alacritty fallback (only on
  allocation failure) is reported on the service's stderr.
- Not surfaced yet: Kitty graphics, grapheme clustering by default (mode 2027),
  SGR-pixel and urxvt mouse formats (also unsupported with alacritty).
- No CI job yet (PLAN Phase 2: cache the library by its sha256).
