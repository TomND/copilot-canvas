# GPU client (Phase 4 MVP, Phase 5 parity, Phase 6 hardening)

`copilot-canvas-gui.exe` is the native GPU front end of Copilot Canvas: the
same canvas, sidebar, dialogs and terminals as the TUI, drawn with wgpu 30,
winit 0.30.13 and glyphon 0.12 (the stack chosen in `docs/GPU-SPIKE.md`). It
runs canvas-core's `App` unchanged as its state machine and talks to the same
terminal service over the same protocol, locally (named pipe) or remotely
(TCP: `--remote loopback` or `HOST:PORT`). Phase 5 brought it to parity with
the TUI (every row of PLAN §8, checked by `selftest --client gui --parity`;
see `docs/PARITY.md`) and added inline images, taskbar progress, flashing and
toasts, file drag and drop, and several windows. Phase 6 added damage frames,
GPU device-lost recovery and a software-renderer fallback, DPI changes,
saved window placement, a UI Automation tree (AccessKit), crash-log details,
the GUI load probes and `selftest --client gui --hardening`.

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
   RenderMode::Gpu                           overlay Buffer (dialogs)├─▶ Scene (10 layers of quads + text) ──▶ wgpu
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

**Damage frames.** Each window keeps its last scene and, on the GPU, each
layer's quad instances and prepared glyphon text (per window: several
windows share the device and the glyph atlas, not the prepared layers). A
service frame that only changes a visible terminal's contents (not its scroll
position, not its first output) does not ask for a full frame: it marks the
tile damaged. The next frame then skips the core's view, the canvas and the
chrome, finds the links in the damaged terminals again
(`ui::rescan_tile_links`), repaints the terminal and HUD layers only, uploads
their quads and prepares their text; every other layer is drawn from what is
already on the GPU. The glyph atlas is trimmed only on full frames (a window
whose text was prepared before another window's trim prepares all its layers
again), so cached layers never point at evicted glyphs. Anything else (input,
a status change, a spinner step, a resize) is a full frame as before.

**Inline images.** The Ghostty engine keeps Kitty graphics (storage limit
64 MiB per terminal; only inline data: the file, temporary-file and
shared-memory mediums are off so a program cannot make the service read
files; PNG payloads are decoded with the `png` crate). Windows' inbox
ConPTY drops APC sequences, so Kitty commands written by a program never
reach the engine; OSC sequences pass, and the host turns iTerm2 inline
images (`OSC 1337;File=inline=1;width=…;height=…:<base64 PNG>`) into the
equivalent Kitty placement (cursor unmoved: ConPTY's cursor is the one that
counts; programs leave room below the image). Only strict base64 bodies are
translated, a sequence split across reads is held and only its new bytes are
searched (linear time, at most 32 MiB). Images whose side exceeds 8192 px or
with more than 4096 × 4096 pixels are never kept or sent. Snapshots list the
render state's placements; the service sends each image once per client
(`ServiceMsg::Image`, PNG, at most 16 MiB) and the viewport's placements
(`ServiceMsg::Placements`: cell, size and offset in the host's 8 × 16 px cell
geometry, source rectangle, z), only to clients that set `CAP_IMAGES` (the
GUI; the TUI does not). Images are PNG-encoded on two encoder threads (never
on the frame pump), at most every 250 ms per changing image, and the encoded
bytes are shared by every client (not copied per client); each client has at
most 32 MiB of image messages queued, more wait their turn while it keeps the
pixels it has. A client may forget images no placement uses; they are sent
again when placed again. The GUI uploads each image once as a texture (keyed
by a serial per received image), draws placements scaled to its own cells
below the text (z < 0) or above it, clipped to the tile, and frees textures
not drawn for 20 s. The texture cache never passes 256 MiB: before an upload
the least recently drawn textures not used in that frame are freed, and an
image that still does not fit is skipped. Images wider or taller than the
device allows (`max_texture_dimension_2d`, at most 8192) are dropped, and a
texture the GPU refuses (an error scope around its creation) drops only that
image. `copilot-canvas
kitty-demo` prints a test image (magenta and green halves, 8 × 2 cells);
`kitty-demo apc` prints the raw Kitty command instead.

**Pasted screenshots.** When Ctrl+V or Alt+V goes to a terminal while the
clipboard holds an image (PNG, `CF_DIBV5` or `CF_DIB`), the key goes to the
agent as before (it attaches the image) and the GUI shows a thumbnail of the
image with its size at the terminal's top right for 5 s.

**File drag and drop.** Files dropped on a terminal are pasted as quoted
paths (as Windows Terminal does). In remote mode they are uploaded first
(`Request::Upload` in 4 MiB pieces, at most 50 MB per file, into
`<home>/uploads/<date>/` on the service's machine, the date the upload
started; the name is claimed with create-new, so an existing file is never
replaced and a taken name becomes `name (2).ext`; Windows device names such as
`CON` or `COM1` get a `_` prefix; unfinished pieces older than a day are
removed) and the remote paths are pasted. A folder dropped on the canvas opens
the new-session dialog in it (local mode).

**Drawing (`scene.rs`, `quads.rs`, `chrome.rs`, `canvas.rs`, `tiles.rs`).** A
frame is 10 layers (base chrome · canvas: background, groups, connectors ·
three box levels · canvas top · terminals · chrome over content · dialogs ·
HUD), each drawn as quads, then text, then overlay quads.
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
  their title row (a selected group: a 3 px outline over a light tint of its
  colour), connectors, the loopback arc with its moving dot (≤ 15 fps),
  pulses, the halo around the active terminal's box (heavier while typing),
  the rubber band. Labels (Segoe UI) are shaped once and scaled with the
  zoom; below a readable size a box shows one compact line, then only its
  shape and a status dot. The dot grid thins out when zoomed out. A box and
  its labels share a layer: a box that overlaps an earlier one goes one box
  level up (three levels), so its fill covers the labels underneath; selected
  boxes are drawn above all of them.
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

**Crash log (`crashlog.rs`).** Panics on any thread (the UI thread and
helpers alike) are appended to `<home>\crash.log` with the build id, the
thread's name, the number of windows, the GPU adapter and the last test-hook
command, then a backtrace; the process exits with 101 and the sessions keep
running in the service. GPU and driver events go to the same file with the
adapter's name, type, backend and driver: a hardware adapter or device that
cannot be opened, a lost device or surface, uncaptured device errors, and
each rebuild (the same text at most every 10 s).

**GPU device loss and software rendering (`gpu.rs`).** wgpu's device-lost
callback and fatal uncaptured errors (out of memory, internal) set a flag and
wake the loop; a present that finds the device gone does the same. The shell
then drops every window's surface and the device, opens a new device
(adapter, pipelines, glyph atlas, text renderers) with the first window's
surface, creates the other windows' surfaces and redraws everything; image
textures are uploaded again when next drawn (the decoded pixels stay in the
views). A failed rebuild is retried every 2 s. A lost surface alone (a
monitor or driver change) is recreated in place; an outdated one is
reconfigured. When no hardware adapter or device works the GUI uses Windows'
software rasterizer (WARP, `force_fallback_adapter`) and says so in a notice;
when that fails too it exits with "cannot draw: no usable GPU and no software
renderer (…). The terminal UI shows the same canvas and sessions without a
GPU: run `copilot-canvas`." (`CANVAS_GUI_TEST_GPU=nohw|none` refuses adapters
for the tests.)

**DPI changes.** `ScaleFactorChanged` (a window moved to a monitor with
another scale) recomputes the cell metrics (whole-pixel cell width, font size
to match), lays the cells out again for the window's size and keeps the world
point at the canvas's centre there (for 0.8 s, across the resize winit sends
next). With `--test-hooks --size`, the grid stays fixed and the window
follows its new pixel size.

**Window placement (`placement.rs`).** Each window's outer position and inner
size (of the restored window), maximized state and monitor name are saved to
`<home>\prefs\<client>-window.json` (`<client>`: the instance, or
`<instance>-windowN` for later windows) a second after they change and when
the window closes, and restored when it opens. A placement whose monitor is
gone, or less than 80 px of whose title bar would be on a monitor's work area
(from winit's `available_monitors` and `GetMonitorInfoW`), is moved: kept on
another monitor it overlaps, else centred on the primary monitor's work area;
it is never larger than the work area. `--size` decides the size when given.

**UI Automation (`access.rs`).** Through AccessKit (`accesskit` 0.25,
`accesskit_winit` 0.34 on winit 0.30), Windows UIA sees the window's
structure, built from the same `View`, canvas model and layout the renderer
draws: the toolbar (buttons by their text, the status counts), the activity
bar (tabs, the active one selected), the sidebar (the session tree's rows as
tree items with their status, other views' lines as list items), the canvas
(groups, and boxes as buttons whose full description is the status line), the
terminal tiles (UIA documents named by their header, the visible text as
their value), the status bar, the connection banner, and the open dialog or
menu (buttons, menu items, list rows, fields, toggles). Focus follows the
core's (a dialog, the focused terminal, the selected box or row). The tree is
built only while a UIA client listens, at most every 150 ms after a frame;
Click, Focus and UIA's Invoke press the element (a click at its centre). The
`tree` test hook includes the same nodes (`access`), and the `access` hook
returns them alone.

## 2. Controls

The keyboard map is the TUI's (`?` shows it). GUI specifics:

| Action | GUI |
|---|---|
| Zoom | Ctrl+wheel or Alt+wheel: continuous, around the pointer (1/8 of a doubling per notch); toolbar − / + and the `-` / `+` keys step 25 % · 50 % · 100 % · 200 %; `F` fits everything (at most 100 %); `C` / Home centres the selection |
| Pan | drag empty canvas or middle-drag, pixel-smooth; wheel / Shift+wheel; Ctrl+arrows |
| Move | drag a box (its subagents follow) or a group title, pixel-smooth; the document stores whole units |
| View / start | a click (canvas or sidebar) only views a box: its live terminal, else its details panel; a double-click, Enter, the panel's **▸ Start** / **▸ Resume** or the box menu starts it. An exited terminal stays on screen while it has the focus |
| Select a group | click its empty background (a drag there still pans) or its outline, up to about half a cell outside it (`BORDER_SLACK`); the innermost group wins; the selected group is drawn heavier |
| Inspect | the box menu's or details panel's **Inspect transcript**: a read-only modal (wheel scrolls, Copy, Close); a finished OpenCode child offers **Resume as subsession** there |
| Remove | right-click on a selected box (or Del) with several selected: "Remove N from canvas" after a confirmation; running sessions stay; Ctrl+Z restores |
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
- **ViewModel snapshots** (PLAN §10 "Core";
  `crates/canvas-core/tests/view_snapshots.rs`, part of `cargo test`):
  scripted states built in-process (a detached connection, no backend,
  synthetic `SessionSummary`/`TermInfo` values, fixed paths and animation
  tick): the demo canvas at 100, 50 and 25 % and at a continuous 70 % with a
  fractional origin; every status (working from history and from OSC 9;4,
  needs you, done · review, idle, stopped, failed three ways, open elsewhere,
  running, preparing); subagents (working, finished, failed), a subsession's
  dashed edge, a loop and a queued message; groups with lit and dim folder
  chips; a selection with a rubber band; the new-session dialog (Agent row
  with an agent that is not installed); the MCP picker with a preset; the
  sidebar filter; two tiled terminals. Each `tests/snapshots/<state>.snap`
  holds the TUI screen (`RenderMode::Cells`) as text and the GPU view
  (`RenderMode::Gpu`) as `View::dump()`: boxes with rect, name, status,
  icon, labels, border and styles, edges with their routes, groups with
  chips, loops, halo, rubber band, tiles, cursor and modal, plus the dialog
  overlay. A mismatch writes `<state>.snap.new` and fails with a line diff;
  `UPDATE_SNAPSHOTS=1 cargo test -p canvas-core --test view_snapshots`
  accepts the new texts.
- `copilot-canvas selftest --client gui [--remote loopback]` runs the TUI
  self-test's 31 steps against the GUI in a 170 × 46 cell window. A `Ui`
  trait has two drivers: the TUI's hidden pseudoconsole and `GuiDriver`,
  which starts `copilot-canvas-gui.exe --test-hooks --size 170x46` and turns
  the terminal input the scenario sends (keys, Alt+digits, Ctrl+letters, SGR
  mouse reports) into hook events at cell centres. `CANVAS_GUI_SCREENS=DIR`
  saves PNGs at six steps.
- `copilot-canvas bench --client gui [--seconds S] [--remote loopback]`
  measures §4 below over the hooks (`CANVAS_GUI_SCREENS` also applies). The
  GUI ports of the TUI's load probes:
  - `bench --client gui --stress [--sessions N --agents M]` (default 100 and
    12): N live sessions from synthetic histories (this process holds their
    in-use locks), M subagents each, 30 of them appending events every 50 ms
    (tools, new subagents, permission prompts); frames while idle, while
    panning and with everything in view (also as forced full frames), the UI
    thread's round trip, CPU and memory;
  - `bench --client gui --perf-canvas FILE|synthetic`: pans a copy of a
    canvas file at 100, 50 and 25 % (`synthetic`: 94 sessions in 9 groups
    with subagents and subsessions);
  - `bench --client gui --burn N`: keystroke → present (real `WM_KEYDOWN`s)
    through an echo terminal while N sessions burn CPU, once with the
    service's priority manager and once with `COPILOT_CANVAS_NO_PRIORITY=1`.
- `copilot-canvas selftest --client gui --hardening`: nine checks with a table
  (about 35 s): the toolbar's counts at 170 columns, overlapping boxes, a
  simulated and a real (destroyed) device loss with identical PNGs before and
  after, a DPI change and back (identical PNG, centre kept), window placement
  across restarts and a gone monitor, the UIA tree read with the
  `IUIAutomation` COM API (and "+ New" pressed through UIA's Invoke pattern),
  crash reports from the UI thread and a helper thread, the WARP fallback and
  the no-adapter error, damage frames while four terminals stream.
- `copilot-canvas selftest --client gui --parity` runs one check per row of
  PLAN §8 plus the GUI-only features, in a sandbox with a stand-in Copilot
  CLI, and prints the table (`docs/PARITY.md`).
- `copilot-canvas selftest --client gui --golden [--update]`: golden
  screenshots with a tolerant image diff (about 15 s). The GUI runs at
  170 × 46 cells on scripted states that depend neither on the machine's data
  nor on the clock: the demo canvas (`demo-canvas`), zoomed out to 50 %
  (`demo-zoom-50`), a box's context menu (`demo-context-menu`), the
  new-session dialog (`demo-new-session-dialog`; the default agent is pinned
  with `COPILOT_CANVAS_AGENT`), two tiled PowerShell terminals that clear the
  screen, print fixed text (16, 256 and RGB colours, bold, dim, italic,
  inverse, strikethrough, overline, single/double/curly/dotted/dashed and
  coloured underlines, emoji, CJK, box and block characters) and set the
  prompt to `PS> ` (`terminals-tiled`), and the parity sandbox's canvas
  with every status, a subagent, loops and the automations group
  (`all-statuses`). Animations hold one frame (the `freeze` hook), notices
  are waited out, and a capture counts once the screen text has been still
  for a second and two renders in a row are identical. Masked (hatched in
  the stored goldens): the service's figures at the status bar's right end,
  folders inside the temporary sandbox (details panel, dialog field) and
  automations' next-run times ("next 7d ago"). Each capture is compared with
  `tests/golden/gui/<name>.png`: a pixel differs when a colour channel moved
  by more than **24**; at most **0.5 %** of the compared pixels may differ;
  and no **8 × 8** block's mean luminance may move by more than **16**, which
  catches text that disappears or appears while being far less than 0.5 %
  of the image. A failure writes `<name>.actual.png` and `<name>.diff.png`
  (differing pixels red, failing blocks outlined yellow, masks blue) next to
  the golden and prints the statistics; `--update` rewrites the goldens
  (review them before committing). `CANVAS_GOLDEN_DIR` points elsewhere.
  The goldens belong to the reference machine (RTX 3070, 125 % DPI: 1700 ×
  1012 px, Cascadia Mono, Segoe UI): another DPI changes the size, another
  GPU or font version the anti-aliasing. Three runs in a row matched
  pixel for pixel (0 differing pixels); changing ANSI green in
  `canvas-gui/src/theme.rs` failed `terminals-tiled` with 0.156 % differing
  pixels (under the fraction) and 26 blocks over the luminance limit.
- `CANVAS_GUI_TRACE=1` prints start-up milestones (and GPU events) on stderr.

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
| `{"cmd":"tree"}` | automation tree: click targets with their text, canvas boxes (status, border, prefix, loop, drawing `level`) and groups with screen rects, tiles (tint, image placements as cell rects), links, terminal selection, notice, focus, modal, paste preview, and `access`: the UI Automation nodes |
| `{"cmd":"access"}` | the UI Automation nodes (role, name, description, value, rect, children, focus) and whether a UIA client listens |
| `{"cmd":"png","path":"…"}` | renders the current frame offscreen to a PNG (works on a locked workstation) |
| `{"cmd":"freeze","tick":0}`, `{"cmd":"freeze"}` | animations (spinners, loop dots, pulsing borders) hold frame `tick`; without it they run again |
| `{"cmd":"size"}`, `{"cmd":"perf","reset":true}` | cells, cell size, DPI, adapter (and details, `software`), present mode; frame statistics (all, full and damage frames), round trips, CPU, memory, first paint, textures, GPU rebuilds |
| `{"cmd":"device-lost"}`, `{"cmd":"device-lost","destroy":true}` | rebuilds the GPU objects as after a lost device (`destroy`: the device is really destroyed first); answers the time taken |
| `{"cmd":"dpi","scale":1.5}` | a DPI change to `scale` (without: the window's own) |
| `{"cmd":"place","x":…,"y":…,"width":…,"height":…,"maximized":bool}`, `{"cmd":"placement"}` | moves, sizes or maximizes the window; its position, size, monitor, the tracked and saved placement, the monitors' work areas |
| `{"cmd":"platform","history":true}` | title, bells, flashes, progress and taskbar state, toasts asked for and their status, the Action Center count, the hooks' clipboard |
| `{"cmd":"clipboard","text":"…"}`, `{"cmd":"clipboard","image":{"width":64,"height":40,"color":[0,0,255]}}`, `{"cmd":"clipboard","clear":true}` | the hooks' clipboard (stands in when Windows' is unavailable) |
| `{"cmd":"new_window"}`, `{"cmd":"windows"}`, `{"cmd":"close_window","window":1}` | opens a window (answers its index once it is up), lists them (with their `hwnd`), closes one |
| `{"cmd":"post_keys","text":"xxx","interval_ms":120}` | real WM_KEYDOWN messages to the window (latency measurement) |
| `{"cmd":"panic"}`, `{"cmd":"panic_thread"}` | crashes on purpose on the UI thread or a helper thread (the crash-log checks) |
| `{"cmd":"quit"}` | saves and exits every window (detach); with `"window"`, closes that one |

## 4. Measured (PLAN §9)

### Phase 6 (damage frames, the GUI load probes)

Release build, RTX 3070, Windows 11, 12 logical CPUs, 125 % DPI, 200 × 56
cells (2000 × 1232 px), workstation locked, 2026-10-28. `bench --client gui
--seconds 5`, `--stress`, `--perf-canvas synthetic`, `--burn 24` and
`--burn 48`; p50 / p95 unless noted. "Full" frames rebuild everything,
"damage" frames only the terminal tiles (§1); forced full frames come from
the `redraw` hook every 10 ms.

| Measure | Budget (§9) | Measured | |
|---|---|---|---|
| **Damage frame**, 4 streaming tiles at 100 % (CPU: tiles repainted, links rescanned, upload, encode) | ≤ 2 ms | **0.71 / 1.00 ms** (p99 1.10; 280 of 283 frames were damage frames) | ✓ |
| Full frame, the same 4 streaming tiles (forced) | ≤ 6 ms | 1.91 / 2.55 ms | ✓ |
| **Full frame, 100 sessions + 1,200 subagents** (1,304 boxes in view at 11 %) + 4 streaming tiles (forced) | ≤ 6 ms | **4.06 / 4.69 ms** (p50: view 2.03, scene 1.21, encode 0.55) | ✓ |
| `--stress`: 100 live sessions + 1,200 subagents from histories (1,600 boxes with spawned ones), 30 busy, all in view, forced full frames | ≤ 6 ms | **4.14 / 4.81 ms** (p99 5.11) | ✓ |
| `--stress`, the same while the sessions stream (no input; spinners and statuses) | ≤ 6 ms | 4.49 / 5.33 ms, 6.6 % of one core (1,600 animated boxes in view) | ✓ |
| `--stress`, idle at 100 % (a part of the canvas in view) | animations ≤ 15 fps, ≤ 2 % CPU | 2.46 / 3.11 ms per frame, **1.9 %** of one core (spinners at 12.5 fps) | ✓ |
| `--stress`, panning at 100 % while busy: frame · drag → present | ≤ 16 ms per frame | 2.12 / 2.61 ms · **2.39 / 3.00 ms** | ✓ |
| `--stress`, UI responsiveness: a hook round trip answered by the UI thread, idle · everything in view | — | 0.24 / 1.85 ms · 0.25 / 3.53 ms | |
| Panning at 100 % while 4 tiles stream (`bench`): frame · drag → present | ≤ 16 ms | 2.12 / 2.59 ms · 2.24 / 2.74 ms | ✓ |
| `--perf-canvas synthetic` (94 sessions, 9 groups, 233 boxes), panning at 100 / 50 / 25 %: frame p95 · drag → present p95 | ≤ 16 ms | 1.44 / 1.46 / 1.75 ms · 1.61 / 1.60 / 1.91 ms | ✓ |
| **Keystroke → present** (real `WM_KEYDOWN`, echo terminal, 3 tiles streaming), p50 / p95 / p99 | p50 ≤ 8, p99 ≤ 20 ms | **5.17 / 5.66 / 7.24 ms** (Phase 4: 6.7 / 7.7 / 7.8) | ✓ |
| `--burn 24` (24 CPU-burning sessions, 12 logical CPUs), keystroke → present p50 / p95 / p99: priority manager on · off | p50 ≤ 8, p99 ≤ 20 ms | **5.66 / 6.55 / 6.88 ms** · 5.76 / 7.07 / 12.04 ms | ✓ |
| `--burn 48`, the same | p50 ≤ 8, p99 ≤ 20 ms | **5.64 / 6.58 / 6.67 ms** · 5.77 / 6.99 / 8.68 ms | ✓ |
| Idle CPU, 10 live sessions, 1,304 boxes (10 s) | ≤ 1 % | **0.31 %** of one core (3 frames) | ✓ |
| Private memory: 4 tiles + 1,304 boxes · `--stress` 1,600 boxes | ≤ 250 MB | **164 MB** · 162–185 MB | ✓ |
| First paint, process start → first frame: service started by the GUI · running | ≤ 1.5 s | **0.89 s** · 0.44 s | ✓ |
| Remote: reconnect after the network returns · full resync · canvas bandwidth with 4 busy tiles (20-minute soaks, `docs/REMOTE.md`) | ≤ 2 s · ≤ 500 ms · ≤ 200 KB/s | GUI canvas p95 1.06 s, max 1.52 s · max 33 ms · 2.4 KB/s | ✓ |

The burn probe's priority manager matters less here than in the TUI's
measurement (README: p99 1.26 s → 14 ms with 40 burners on a busy 16-core
machine): the GUI raises its own priority, Windows boosts the foreground
window's input, and the echo terminal competes only with the burners; the
priority manager still halves the p99 at 24 burners. Damage frames also
shortened keystroke → present by ~1.5 ms, since the echo is a damage frame.

### Phase 4 (for comparison; every frame was a full frame)

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

`docs/gui-screens/` (offscreen renders through the test hooks; the golden
screenshots the `--golden` check compares against are in `tests/golden/gui/`):
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

Phase 6 (`--hardening` and the probes, same variable):
`toolbar-170-columns.png` (non-demo toolbar at 170 columns with sessions in
every state: the buttons stay whole, "open elsewhere" and the session total
are dropped), `overlapping-boxes.png` (a box overlapping another's labels
covers them), `software-renderer.png` (WARP, with its notice),
`stress-1600-boxes-fit.png` (`--stress`: 100 busy sessions with their
subagents at 8 %) and `perf-canvas-synthetic-25.png` (`--perf-canvas
synthetic` at 25 %).

## 6. Known gaps (after Phase 6)

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
- **Widgets:** dialogs, menus and lists are the TUI's cell layouts drawn
  crisply; no GUI-native widgets, text-field selection with the mouse, or
  scroll bars in lists. The UI Automation tree names their buttons, rows and
  fields from those cells; text fields expose no caret or selection to UIA,
  and terminal text is a plain value (no text pattern).
- **Box levels:** three; a fourth box stacked on three overlapping ones
  shares the top level, so its labels can still mix with the third's.
- **Damage frames** cover terminal output only; the canvas and chrome are
  still rebuilt whole (a spinner step on the canvas is a full frame, 2–5 ms).
  No scissored partial presents: on WARP every frame still rasterizes the
  whole window.
- **Device loss** is tested by rebuilding on request and by destroying the
  device (the driver's own report path); a real TDR (driver reset) or an
  unplugged GPU was not reproduced.
- **Start-up:** GPU device creation (0.38 s) could overlap the service
  connection; shaders are compiled at start.
- **Not verified by hand:** a real IME session (Japanese, Chinese), AltGr on a
  German layout, a real move between monitors of different DPI (the `dpi`
  hook runs the same code), Narrator reading the tree (a UIA client test
  does), an unlocked desktop's vsync, real drag and drop from Explorer (the
  hooks inject the drop).
- Cursor blink, the Copilot-only hollow cursor for unfocused tiles, and
  hinting (text is grayscale, unhinted as in the spike).

