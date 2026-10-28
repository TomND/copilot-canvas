# GPU client (Phase 4 MVP, Phase 5 parity)

`copilot-canvas-gui.exe` is the native GPU front end of Copilot Canvas: the
same canvas, sidebar, dialogs and terminals as the TUI, drawn with wgpu 30,
winit 0.30.13 and glyphon 0.12 (the stack chosen in `docs/GPU-SPIKE.md`). It
runs canvas-core's `App` unchanged as its state machine and talks to the same
terminal service over the same protocol, locally (named pipe) or remotely
(TCP: `--remote loopback` or `HOST:PORT`). Phase 5 brought it to parity with
the TUI (every row of PLAN §8, checked by `selftest --client gui --parity`;
see `docs/PARITY.md`) and added inline images, taskbar progress, flashing and
toasts, file drag and drop, and several windows.

```powershell
cargo build --release                       # builds copilot-canvas.exe and copilot-canvas-gui.exe
target\release\copilot-canvas-gui.exe --demo
target\release\copilot-canvas.exe --gui     # the same, launched from the main program
```

Options: `--demo`, `--perf`, `--cwd DIR`, `--canvas FILE`, `--instance NAME`,
`--home DIR`, `--sessions-dir DIR`, `--remote loopback|HOST:PORT`,
`--test-hooks`, `--size COLSxROWS`. The terminal service is the
`copilot-canvas.exe` next to the GUI: its content hash is the build both sides
compare, and the service is started (and upgraded) from it.

## 1. Architecture

```
canvas-core (unchanged state machine)            canvas-gui
App ──ui::render(app, buf, area, overlay)──▶ chrome Buffer (cells) ─┐
   RenderMode::Gpu                           overlay Buffer (dialogs)├─▶ Scene (7 layers of quads + text) ──▶ wgpu
                                             View { canvas: CanvasModel (world units),
                                                    tiles: [TileModel], cursor, modal, banner } ┘
App::views[id] (TermView, protocol-8 rows) ──────────────────────────▶ tiles painter
winit events ──▶ InputEvent (fractional cell positions) ──▶ App::on_input
```

**Render modes (canvas-core `view.rs`).** `RenderMode::Cells` is the TUI and
draws exactly what it drew before (the TUI selftest and a parity unit test
check it). In `RenderMode::Gpu`, `ui::render`:

- fills the chrome (toolbar, activity bar, sidebar and list views, detail
  panels, tile frames and headers, status bar) into a cell buffer, with the
  canvas region and the terminal grids left blank;
- draws dialogs and menus into a separate overlay buffer;
- describes the canvas as a `CanvasModel` in world units: groups (rect, title,
  colour, selection, drop target, folder chips, automation family labels),
  boxes (kind, rect with the drag offset, name and prefix, status icon, label
  and line, subtitle, border kind and style, attention colours, fill, loop,
  mailbox, selection, active terminal), edges (subagent solid, subsession
  dashed, orthogonal routes), loop arcs, event pulses, the halo, the rubber
  band. Everything comes from the same helpers `draw_canvas` uses
  (`box_attention`, `box_border`, `box_border_style`, `box_label`, …), so
  colours, labels and statuses match the TUI;
- lists the visible tiles (`TileModel`: id, outer and inner rects, focus,
  cursor, Copilot tint, title). Links in terminals are still found on the
  cell text before the grid is blanked, so Ctrl+click works as in the TUI.

`ui_canvas::paint_model` draws a `CanvasModel` back as TUI characters; at
100 % it reproduces `draw_canvas` cell for cell (unit test), and the test
hooks use it for the GUI's text screen.

**Geometry and continuous zoom (canvas-core `app.rs`).** In Gpu mode the
canvas scale is `App::view_scale` (cells per world unit, 1/16 … 2) with a
fractional view origin (`view_frac`); the document keeps integer coordinates
and the nearest discrete zoom (for the TUI). `to_screen_f`/`to_world_f` are
the precise transforms; `canvas_hit_f` hit-tests at the pointer's fractional
cell position (`App::pointer`), so every hit test of the core works at any
scale. Box and group drags move the document in whole units and show the rest
as a sub-unit visual offset (`App::drag_offset`), so they are pixel-smooth.

**The windows (`shell.rs`).** Every window runs its own `App` with its own
service connection and view preferences (`<home>/prefs/<instance>-windowN-…`);
the service's document sync keeps their canvases consistent. They share the
GPU device, the pipelines and the fonts; each has its swap chain, shape and
row caches. Ctrl+Shift+N or the toolbar's ⧉ opens another window (its app
connects on a background thread); closing a window while others are open
detaches it without asking, the last one asks as before, and the process
ends with its last window. Service messages are forwarded by a thread per
connection into a queue (PNG images are decoded to RGBA on the way) and wake
the loop with one coalesced proxy event; the UI drains them with a 4 ms
budget, as the TUI does. App events and test-hook commands arrive as proxy
events too. Ticks run every 40 ms while anything is active (animations,
notices, drags, unsaved changes, a paste preview) and every 400 ms
otherwise. A frame is drawn only when the app needs one: at once after input
(and for the echo of a keystroke in the focused terminal), otherwise paced to
the display's refresh rate. Presentation uses Mailbox (Immediate, then Fifo,
as fallbacks) and never blocks the UI thread. `after_draw` sends
subscriptions, tile sizes and tracking exactly as in the TUI.

**Inline images.** The Ghostty engine keeps Kitty graphics (storage limit
64 MiB per terminal; only inline data: the file, temporary-file and
shared-memory mediums are off so a program cannot make the service read
files; PNG payloads are decoded with the `png` crate). Windows' inbox
ConPTY drops APC sequences, so Kitty commands written by a program never
reach the engine; OSC sequences pass, and the host turns iTerm2 inline
images (`OSC 1337;File=inline=1;width=…;height=…:<base64 PNG>`) into the
equivalent Kitty placement (cursor unmoved: ConPTY's cursor is the one that
counts; programs leave room below the image). Snapshots list the render
state's placements; the service sends each image once per client
(`ServiceMsg::Image`, PNG, at most 16 MiB) and the viewport's placements
(`ServiceMsg::Placements`: cell, size and offset in the host's 8 × 16 px cell
geometry, source rectangle, z), only to clients that set `CAP_IMAGES` (the
GUI; the TUI does not). A client may forget images no placement uses; they
are sent again when placed again. The GUI uploads each image once as a
texture (keyed by a serial per received image), draws placements scaled to
its own cells below the text (z < 0) or above it, clipped to the tile, and
frees textures not drawn for 20 s (at most 256 MiB). `copilot-canvas
kitty-demo` prints a test image (magenta and green halves, 8 × 2 cells);
`kitty-demo apc` prints the raw Kitty command instead.

**Pasted screenshots.** When Ctrl+V or Alt+V goes to a terminal while the
clipboard holds an image (PNG, `CF_DIBV5` or `CF_DIB`), the key goes to the
agent as before (it attaches the image) and the GUI shows a thumbnail of the
image with its size at the terminal's top right for 5 s.

**File drag and drop.** Files dropped on a terminal are pasted as quoted
paths (as Windows Terminal does). In remote mode they are uploaded first
(`Request::Upload` in 4 MiB pieces, at most 50 MB per file, into
`<home>/uploads/<date>/` on the service's machine, under a free name) and
the remote paths are pasted. A folder dropped on the canvas opens the
new-session dialog in it (local mode).

**Drawing (`scene.rs`, `quads.rs`, `chrome.rs`, `canvas.rs`, `tiles.rs`).** A
frame is 7 layers (base chrome · canvas · canvas top · terminals · chrome over
content · dialogs · HUD), each drawn as quads, then text, then overlay quads.
Quads come from one instanced SDF pipeline (rounded rectangles with solid or
dashed borders, line segments with round caps). Text goes through glyphon,
one renderer per layer, from a cache of shaped buffers keyed by content, face
and size.

- *Chrome:* cells on a grid whose cell width is a whole number of pixels and
  whose font (Cascadia Mono, else Consolas) is sized so one advance is one
  cell; runs of simple characters are shaped together, other symbols, emoji
  and wide characters are centred in their cells. Box-drawing and block
  characters (light, heavy, double, dashed, rounded) are drawn as
  pixel-aligned rectangles and arcs, so frames join seamlessly.
- *Canvas:* rounded boxes with status borders (needs you: heavy gold; done ·
  review: double blue with a tinted body; failed automation: pulsing
  vermillion; selected: heavy white; passive: dashed), group outlines with
  their title row, connectors, the loopback arc with its moving dot (≤ 15 fps),
  pulses, the halo around the active terminal's box (heavier while typing),
  the rubber band. Labels (Segoe UI) are shaped once and scaled with the
  zoom; below a readable size a box shows one compact line, then only its
  shape and a status dot. The dot grid thins out when zoomed out. Selected
  boxes are drawn above the others.
- *Terminals:* protocol-8 rows (grapheme clusters, engine widths, colours,
  bold, italic, dim, inverse, hidden, strikethrough, overline, underline
  styles single/double/curly/dotted/dashed with their colour), OSC 8 and
  plain-text links underlined on hover or with Ctrl, block / underline / bar
  / hollow cursors, selection, Copilot's user-message tint, a scroll thumb
  when scrolled back, and the IME composition at the cursor. Row layouts are
  cached by row content; only changed rows are rebuilt.

**Fonts (`text.rs`).** Only the needed font files are loaded (Cascadia Mono,
Consolas, Segoe UI and Symbol, Segoe UI Emoji, Microsoft YaHei, Yu Gothic,
Malgun Gothic, plus per-user fonts); cosmic-text falls back between them per
character. They load on a thread while the service connects.

**Input (`input.rs`).** winit keys become core `KeyEvent`s from the logical
key (all modifiers but Ctrl applied; AltGr compositions arrive composed);
text of several characters and IME commits become `InputEvent::Text` (a
terminal gets the characters, a field inserts them). Mouse positions are
fractional cells; wheel pixel deltas accumulate into notches (three rows
each). Ctrl+V and Shift+Insert outside a terminal paste the clipboard into
the focused field; in a terminal the core pastes (or sends Ctrl+V for an
image) as in the TUI.

**Platform (`canvas-core platform.rs`, `canvas-gui platform.rs`).** The core
reaches the machine through `trait Platform`: clipboard text, "clipboard has
an image", opening links and paths, bells, taskbar progress, the window
title, program notifications and sessions that start to need you. The
TUI's `TerminalPlatform` keeps today's behaviour (bell and OSC 9;4 bytes for
the outer terminal). The GUI mirrors the canvas to the taskbar button's
progress (ITaskbarList3: busy while a session works, paused/yellow while one
needs you, from the same `mirror_progress` as the TUI's OSC 9;4), puts the
number of sessions that need you in the title, flashes the taskbar button
(FlashWindowEx) for bells and sessions that start to need you while the
window is in the background, and shows Windows toasts for program
notifications (OSC 777 / OSC 9, with their title and body:
`ServiceMsg::Notification` to `CAP_NOTIFY` clients) and for sessions that
start to need you. Toasts use WinRT's `ToastNotificationManager` from a
background thread. An unpackaged app needs a registered AppUserModelID: the
GUI registers `CopilotCanvas.Gui` under
`HKCU\Software\Classes\AppUserModelId` (display name; no Start-menu shortcut)
and gives the process that id. No toast is shown while the window is in
front, the same text is shown once (however many windows are open), and a
toast that fails still leaves the flashing button.

**Connection and upgrades.** Local pipe by default, `--remote` as the TUI;
the offline banner, reconnect with backoff and blocked input come from the
core. A local service of another build is restarted when it runs no
sessions; otherwise the window opens with a dialog (restart and resume, or
later / quit when its protocol is older). The toolbar's "⟳ Update service"
works as in the TUI. The service runs from a per-build copy of the
`copilot-canvas.exe` next to the GUI (`paths::set_build_exe`).

**Crash log.** Panics are appended to `<home>\crash.log`; the sessions keep
running in the service.

## 2. Controls

The keyboard map is the TUI's (`?` shows it). GUI specifics:

| Action | GUI |
|---|---|
| Zoom | Ctrl+wheel or Alt+wheel: continuous, around the pointer (1/8 of a doubling per notch); toolbar − / + and the `-` / `+` keys step 25 % · 50 % · 100 % · 200 %; `F` fits everything (at most 100 %); `C` / Home centres the selection |
| Pan | drag empty canvas or middle-drag, pixel-smooth; wheel / Shift+wheel; Ctrl+arrows |
| Move | drag a box (its subagents follow) or a group title, pixel-smooth; the document stores whole units |
| Pointer | hand over buttons and links, resize arrows over the dividers, text beam over terminals |
| Paste | Ctrl+V / Shift+Insert: into a terminal (images as in the TUI, with a thumbnail of the image) or the focused text field |
| Drop files | on a terminal: quoted paths pasted (remote mode: uploaded first); a folder on the canvas: new session there |
| New window | Ctrl+Shift+N or the toolbar's ⧉; closing one of several windows detaches it |
| IME | composition drawn at the terminal cursor or field; committed text typed |
| Close button | as Ctrl+Q: detach or stop all (the last window) |
| `--perf` | frame time (view, scene, encode), present and acquire, input → present, keystroke → echo → present, service round trip |

## 3. Testing

- `cargo test --workspace`: canvas-core has model tests (the model painted
  as text equals the TUI's canvas at 100 %; zoom steps and continuous zoom
  around the pointer; pixel hit testing; whole-unit drags with visual
  offset) and drop/upload tests; canvas-gui tests box drawing, row layout
  (clusters, wide cells, underline styles, inverse, hidden, links), key and
  wheel mapping, colours, grid metrics, latency bookkeeping, the platform
  (titles, toast deduplication), PNG and clipboard bitmap decoding and
  thumbnails; canvas-host tests Kitty placements and pixels, the iTerm2
  bridge and the refused file medium; the protocol tests which clients get
  images and notifications.
- `copilot-canvas selftest --client gui [--remote loopback]` runs the TUI
  self-test's 26 steps against the GUI in a 170 × 46 cell window. A `Ui`
  trait has two drivers: the TUI's hidden pseudoconsole and `GuiDriver`,
  which starts `copilot-canvas-gui.exe --test-hooks --size 170x46` and turns
  the terminal input the scenario sends (keys, Alt+digits, Ctrl+letters, SGR
  mouse reports) into hook events at cell centres. `CANVAS_GUI_SCREENS=DIR`
  saves PNGs at six steps.
- `copilot-canvas bench --client gui [--seconds S] [--remote loopback]`
  measures §4 below over the hooks (`CANVAS_GUI_SCREENS` also applies).
- `copilot-canvas selftest --client gui --parity` runs one check per row of
  PLAN §8 plus the GUI-only features, in a sandbox with a stand-in Copilot
  CLI, and prints the table (`docs/PARITY.md`).
- `CANVAS_GUI_TRACE=1` prints start-up milestones on stderr.

**Test hooks.** With `--test-hooks` the GUI prints a pipe name
(`\\.\pipe\copilot-canvas-gui-test-<pid>`, owner-only, local only) and takes
one JSON command per line. Commands act on the first window unless they name
one (`"window": n`, the creation index):

| Command | Effect / answer |
|---|---|
| `{"cmd":"key","key":"Enter"}`, `{"cmd":"key","char":"a","mods":["ctrl"]}` | one key |
| `{"cmd":"keys","keys":[…]}` | a batch, as one console read |
| `{"cmd":"text","text":"…"}`, `{"cmd":"paste","text":"…"}` | IME-style text, a paste |
| `{"cmd":"ime","preedit":"…"}`, `{"cmd":"ime","commit":"…"}` | IME composition and commit |
| `{"cmd":"mouse","kind":"down\|up\|move\|drag","button":"left","x":40.5,"y":7.5,"mods":[]}` | mouse at fractional cells |
| `{"cmd":"wheel","dir":"up\|down\|left\|right","x":…,"y":…,"mods":["ctrl"]}` | one wheel notch |
| `{"cmd":"drop","paths":["…"],"x":…,"y":…}` | files dropped at a cell |
| `{"cmd":"screen"}` | `lines`: the screen as text (chrome, canvas as TUI characters, terminal text, dialogs) |
| `{"cmd":"tree"}` | automation tree: click targets with their text, canvas boxes (status, border, prefix, loop) and groups with screen rects, tiles (tint, image placements as cell rects), links, terminal selection, notice, focus, modal, paste preview |
| `{"cmd":"png","path":"…"}` | renders the current frame offscreen to a PNG (works on a locked workstation) |
| `{"cmd":"size"}`, `{"cmd":"perf","reset":true}` | cells, cell size, DPI, adapter, present mode; frame statistics, round trips, CPU, memory, first paint, textures |
| `{"cmd":"platform","history":true}` | title, bells, flashes, progress and taskbar state, toasts asked for and their status, the Action Center count, the hooks' clipboard |
| `{"cmd":"clipboard","text":"…"}`, `{"cmd":"clipboard","image":{"width":64,"height":40,"color":[0,0,255]}}`, `{"cmd":"clipboard","clear":true}` | the hooks' clipboard (stands in when Windows' is unavailable) |
| `{"cmd":"new_window"}`, `{"cmd":"windows"}`, `{"cmd":"close_window","window":1}` | opens a window (answers its index once it is up), lists them, closes one |
| `{"cmd":"post_keys","text":"xxx","interval_ms":120}` | real WM_KEYDOWN messages to the window (latency measurement) |
| `{"cmd":"panic"}` | crashes on purpose (the crash-log check) |
| `{"cmd":"quit"}` | saves and exits every window (detach); with `"window"`, closes that one |

## 4. Measured (PLAN §9)

Release build, RTX 3070, Windows 11, 125 % DPI, 200 × 56 cells (2000 × 1232 px),
workstation locked (as in the spike). `bench --client gui --seconds 6`: two
local runs (shown as run 1 · run 2 where they differ) and one over
`--remote loopback`:

| Measure | Budget | Local | Loopback TCP |
|---|---|---|---|
| Keystroke → present (echo terminal, 3 tiles streaming), p50 / p95 / p99 | p50 ≤ 8, p99 ≤ 20 ms (remote: RTT + ≤ 10) | **6.7 / 7.7 / 7.8 ms** · 7.2 / 7.8 / 8.0 ms | **7.0 / 7.8 / 7.9 ms** (RTT 1.3 ms) |
| Input → present (the frame showing an input), p50 / p95 | — | 2.3 / 3.0 ms · 2.6 / 3.0 ms | 2.7 / 3.1 ms |
| Full frame: 100 sessions + 1,200 subagents (1,304 boxes, all in view) + 4 streaming tiles, p50 / p95 | ≤ 6 ms | **4.3 / 5.6 ms** · 4.5 / 5.2 ms (p50: core view 2.5, scene 0.8, encode 0.7) | 4.6 / 5.1 ms |
| Frame with 4 streaming tiles at 100 % (no damage tracking: full frames) | ≤ 6 ms full, damage ≤ 2 ms | **2.0 / 2.7 ms** · 2.2 / 2.7 ms | 2.4 / 3.0 ms |
| Panning at 100 % while tiles stream, frame p50 / p95 (drag → present) | ≤ 16 ms | **2.4 / 3.0 ms** (2.6 / 3.3 ms) · 2.2 / 2.8 ms | 2.2 / 2.8 ms (2.4 / 3.2 ms) |
| Idle CPU, 10 live sessions, 1,304 boxes (10 s) | ≤ 1 % | **0.5 %** of one core (3 frames: service stats) | 0.15 % |
| Private memory, 4 tiles + 1,304 boxes | ≤ 250 MB | **153–174 MB** | 177 MB |
| First paint, process start → first frame, service started by the GUI | ≤ 1.5 s | **0.94 s** · 0.96 s | 0.90 s |
| First paint, service running | — | 0.52 s · 0.57 s | 0.55 s |
| Self-test, start → canvas visible to the driver (incl. service start) | — | 0.96–1.44 s | 0.91 s |

Where start-up goes (`CANVAS_GUI_TRACE`, cold service): service started and
connected 0.50 s (fonts load meanwhile), window 0.03 s, GPU device and
pipelines 0.38 s, first frame 0.02 s.

## 5. Screenshots

`docs/gui-screens/` (offscreen renders through the test hooks):
`terminal-and-halo.png` (a live shell, the active box's heavy halo),
`tiled.png` (three tiles, a selected group), `zoom-50.png`,
`context-menu.png`, `new-session-dialog.png`, `automations-loops.png`
(automation boxes, a loopback arc, the details panel),
`bench-streaming-tiles.png` and `bench-1300-boxes-fit.png` (the benchmark
canvas: 4 streaming tiles; 1,304 boxes in view at 11 %),
`ime-preedit-and-perf.png` (an IME composition, the `--perf` overlay),
`offline-banner.png` (the service just went away).

The self-test's six PNGs and the benchmark's two are regenerated with
`CANVAS_GUI_SCREENS=docs\gui-screens`; the IME and offline shots were taken
by hand through the hooks. The parity suite (`--parity`, same variable)
saves `inline-image.png` (a Kitty image from `kitty-demo` in a shell, 8 × 2
cells; the paste preview at the top right), `paste-image-preview.png` (the
thumbnail of a pasted 64 × 40 image over the stand-in Copilot, in mouse mode
with forwarded clicks), `user-message-tint.png` (named
`parity-user-message-tint.png` by the suite) and `second-window.png` (a
second window on the same canvas after a rename made in the first).

## 6. Known gaps (Phase 6 and later)

- **Kitty graphics through ConPTY:** Windows' inbox ConPTY drops APC, so only
  iTerm2 inline PNG images (an OSC) reach the engine from programs; a raw
  Kitty command needs a pseudoconsole that passes APC through (a newer
  `conpty.dll`). Programs must leave rows free below an image (ConPTY does
  not know its size). Animated Kitty images show their current frame (the
  engine's animation tick is not driven); JPEG and GIF files are not shown.
- **Toasts:** clicking one does not bring the window forward (no COM
  activator is registered); Focus Assist decides whether they pop up.
- **Clipboard on a locked workstation:** Windows refuses it; copy and paste
  fail with a notice (the test hooks use a clipboard of their own then).
- **Toolbar crowding:** at narrow widths the status counts on the right can
  cover the last toolbar buttons (as in the TUI).
- **Widgets:** dialogs, menus and lists are the TUI's cell layouts drawn
  crisply; no GUI-native widgets, text-field selection with the mouse, or
  scroll bars in lists. Accessibility (UI Automation) is Phase 6; the `tree`
  hook is its test-side stand-in.
- **Overlapping boxes:** labels are drawn above all box shapes of their
  layer, so a lower box's label can show through an overlapping box (selected
  boxes, the ones you drag, are drawn above the rest).
- **No damage tracking:** every frame is a full frame (2–3 ms with four
  streaming tiles). Scissored partial redraws would matter on WARP.
- **Start-up:** GPU device creation (0.38 s) could overlap the service
  connection; shaders are compiled at start.
- **Not verified by hand:** a real IME session (Japanese, Chinese), AltGr on a
  German layout, per-monitor DPI changes, an unlocked desktop's vsync, real
  drag and drop from Explorer (the hooks inject the drop).
- Cursor blink, the Copilot-only hollow cursor for unfocused tiles, and
  hinting (text is grayscale, unhinted as in the spike).

