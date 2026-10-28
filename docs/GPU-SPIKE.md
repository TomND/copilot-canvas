# GPU spike (Phase 3): GPUI vs wgpu + winit + glyphon

Date: 2026-10-27 · Branch `p3-spike` · Plan reference: PLAN.md §3, §4 D3/D4, Phase 3, §9.

## 1. Decision (D3)

**Build the GPU client on wgpu + winit + glyphon.**

The results are not close on the criteria that define this product, so PLAN's tie-break ("default: GPUI") does not apply:

| Criterion (PLAN D3) | GPUI | wgpu + winit + glyphon | Verdict |
|---|---|---|---|
| Four 200×60 streaming tiles | 6.5 ms CPU per frame, 44 % of a core | 1.3 ms per frame, 13 % of a core | **wgpu, by 5× per frame and 3.4× in CPU** |
| 300 boxes, 30 groups, 3,000 subagents while panning and zooming | p50 2.4 ms, p95 6.3 ms, 60 fps | p50 1.6 ms, p95 5.9 ms, 59 fps | tie |
| Keystroke → present (PLAN §9: p50 ≤ 8 ms, p99 ≤ 20 ms) | p50 34.7 ms, p95 50.9 ms | p50 9.9 ms, p95 11.7 ms | **wgpu**. GPUI misses the budget by 2.5–4×. |
| IME and AltGr | Complete IMM32 integration (`EntityInputHandler`), AltGr through `prefer_character_input` | winit `Ime` events plus `set_ime_cursor_area`; AltGr filtering on AltGr layouts | GPUI slightly ahead; both work in the prototypes |
| First paint | 292 ms | 461 ms | GPUI (both within the 1.5 s budget) |
| Memory | 145 MB after 10 s, 189 MB at end | 181 MB after 10 s, 205 MB at end | GPUI (both within the 250 MB budget) |
| Binary size and build | 11.3 MB; 444 crates; 129 s clean build | 8.5 MB; 268 crates; 75 s clean build | wgpu |
| Reuse of `canvas-core` | `conn`, protocol, `TermView` | the same | tie. Reuse depends on `canvas-core`'s API, not on the UI stack (§7). |
| GPU-less Cloud PC (WARP) | 19–26 fps, about 850 % CPU | 8–10 fps for text, 28 fps panning, about 680 % CPU | Neither is usable. The TUI stays the default there (D4). |

Hardware figures are medians of 3 runs on an RTX 3070; details are in §5.

### Why wgpu

- **Streaming terminals and typing are the core workload.** PLAN goal 6 is "no responsiveness regression". GPUI misses the keystroke budget for two structural reasons:
  - **A draw before every key.** When the window is dirty, which it always is while other tiles stream, GPUI's `dispatch_key_event` draws a full frame before it dispatches the key. That adds about 6 ms to every keystroke.
  - **The whole window, every frame.** Each frame rebuilds the scene and costs about 0.3 µs per glyph. View caching (`.cached()`) saved only about 0.6 ms, because cached primitives are still replayed into the new scene.

  Fixing either problem means forking GPUI.
- **With wgpu, latency is our own code.**
  - The first wgpu version used FIFO presentation. The UI thread blocked about 15 ms in `get_current_texture`, which delayed service frames: 33 ms p50.
  - Switching to Mailbox presentation with our own pacing to the display refresh brought it to 9.9 ms p50 and 11.7 ms p95.
  - Of that, 4.7 ms is the service round trip, the same as the TUI bench measures. The client's part is 5.3 ms.
- **Frame cost leaves headroom.** At 1.3–2.3 ms per full frame (4 tiles; or 22,800 visible cells), wgpu meets §9's "≤ 6 ms full frame". A full frame already fits the "≤ 2 ms" damage budget, and scissored partial redraws are possible later. GPUI redraws the whole window every frame and cannot get under about 6 ms with four streaming tiles.

### What we give up, and the conditions attached

- **We write the toolkit parts GPUI provides:**
  - layout;
  - text fields (IME, selection, clipboard);
  - lists, scrolling, menus and dialogs;
  - focus;
  - accessibility (UI Automation via `accesskit`).

  **Condition:** in Phase 4 week 1, prototype the chrome (sidebar, one dialog with a text field, a menu) with **egui on the same wgpu device and winit window**, repainting only on change. Measure idle CPU and typing latency with the same `--measure` driver. If egui fails the budgets, build a small retained widget set on `taffy` and glyphon instead. Add 1–2 weeks to Phases 4–5 for this either way.
- **First paint is slower** (about 460 ms against 290 ms). wgpu translates WGSL with naga and compiles the shaders at startup; GPUI ships HLSL precompiled at build time. Still under budget. Pipeline creation can move off the first frame later.
- **Memory needs a setting.** wgpu's default `MemoryHints::Performance` reserved about 180 MB more private memory: 313 MB right after device creation, against 134 MB with `MemoryHints::MemoryUsage`. **The client must use `MemoryUsage`.**
- **Text is slightly softer.** glyphon renders through swash in grayscale without hinting; GPUI uses DirectWrite. Both are clearly readable (see the screenshots in `docs/spike-results/`). Revisit with hinting or a DirectWrite rasterizer behind glyphon's custom-glyph hook if users notice.
- **Keep drawing behind the `ViewModel`** (PLAN risk table), so the GPUI option stays open if its Windows latency improves upstream.

## 2. What was built

```
spikes/
  shared/            used by both prototypes (#[path] modules), 1,468 lines
    scene.rs         synthetic canvas: 30 groups, 300 sessions, 3,000 subagents,
                     3,330 connectors (630 dashed), camera (pan, zoom at cursor), culling, LOD
    term.rs          xterm palette, protocol colours, bold/italic/underline/strike/inverse/dim/hidden,
                     rows → cell-aligned text segments (wide CJK/emoji = 2 cells), tile layout
    keys.rs          key → protocol Input (AltGr = Ctrl+Alt with text → plain text), PostMessage injection
    service.rs       connects with canvas_core::conn::try_connect, launches and subscribes terminals
    measure.rs       --measure driver: phases, frame statistics, Win32 memory/CPU, JSON report
  gpui-spike/        GPUI 0.3.10 (gpui-pre), 908 lines; warp/build-warp.ps1 for the WARP build
  wgpu-spike/        wgpu 30 + winit 0.30.13 + glyphon 0.12, 1,250 lines (incl. a 200-line SDF quad pipeline)
```

Both prototypes do the same thing:

- **Window.** One per-monitor-DPI-aware window, 1800×1000 logical.
- **Canvas (left half).** Groups as rounded outlines with titles. Sessions as rounded boxes with a title and a status line. Subagents as small boxes on elbow connectors, with dashed connectors and diagonal dashed subsession links.
  - Pan by left-drag or wheel; continuous zoom with Ctrl+wheel around the cursor.
  - Off-screen items are culled. Labels below 4.5 px are hidden.
  - Labels are shaped once and painted at the zoomed size (sizes quantized to 0.25 px).
- **Four terminal tiles (right half, 2×2).** Live `TermView`s of 200×60 terminals from a `copilot-canvas` service instance.
  - Cascadia Mono, falling back to Consolas; xterm colours; bold, italic, underline, strikethrough, inverse and dim.
  - Wide characters take two cells. Cursor: filled when focused, hollow otherwise.
  - Each tile shows the part of its 200×60 grid that fits, following the cursor.
  - F2 switches to a tiles-only layout where all 4 × 200 × 60 cells are drawn at a fitted font size (about 6.5 px).
- **Keyboard.** Focus on one tile (F3 or click cycles); keys go to that terminal.
  - IME preedit is drawn inline at the cursor, with an underline.
  - Committed text is sent as `Input::Paste`, or as `Key::Char` for a single character.
  - AltGr (Ctrl+Alt producing text) is sent as plain text.
- **Damage-driven rendering.** Redraw only on input, service frames or animation. Both measured 0 frames over 10 idle seconds.
- **`--measure`.** Runs the phases in §4 and prints one JSON object (`--out FILE` also writes it).

Only the windowing and drawing code differ. Scene, terminal model, key mapping, service handling and the measurement driver are the same source files.

## 3. Setup and how to run

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
cargo build --release                                   # root: target\release\copilot-canvas.exe (service, noise, echo)
target\release\copilot-canvas.exe service start --instance spike
cargo build --release --manifest-path spikes\gpui-spike\Cargo.toml
cargo build --release --manifest-path spikes\wgpu-spike\Cargo.toml
spikes\gpui-spike\target\release\gpui-spike.exe          # interactive
spikes\wgpu-spike\target\release\wgpu-spike.exe          # interactive
spikes\gpui-spike\target\release\gpui-spike.exe --measure --out gpui.json
spikes\wgpu-spike\target\release\wgpu-spike.exe --measure --out wgpu.json
spikes\wgpu-spike\target\release\wgpu-spike.exe --measure --warp      # DX12 WARP (force_fallback_adapter)
powershell -File spikes\gpui-spike\warp\build-warp.ps1                # GPUI with a WARP switch (see below)
spikes\gpui-spike\target\warp\release\gpui-spike.exe --measure --warp
target\release\copilot-canvas.exe service stop --instance spike
```

**Options**

- `--ime-demo` and `--altgr-demo` synthesize input (§6).
- GPUI only: `--uncached` disables view caching.
- wgpu only:
  - `SPIKE_PRESENT=fifo` selects FIFO presentation.
  - `SPIKE_WGPU_MEMORY=performance` restores wgpu's default allocator hints.
  - `SPIKE_SKIP=text|quads` is a diagnostic that skips one kind of draw.

**How the spikes use the service**

- Each spike launches nine terminals of its own:
  - four `copilot-canvas noise --rate 20`;
  - four `powershell.exe -NoLogo -NoProfile`;
  - one `copilot-canvas echo`.
- Terminal ids are prefixed with the process id. The spike stops them when it exits normally.
- Connection is via `try_connect` only. `connect(start=true)` would copy the spike executable as the service.

**Forcing WARP**

- **wgpu:** `force_fallback_adapter: true` with `Backends::DX12`.
- **GPUI:** there is no switch. It uses the first DXGI adapter that creates a D3D11 device. On a machine without a GPU that adapter is already WARP ("Microsoft Basic Render Driver").
  - To simulate that on this machine, `warp/build-warp.ps1` copies `gpui-pre-windows` 0.3.10 from the cargo registry and adds an 8-line `GPUI_FORCE_WARP` branch (`IDXGIFactory4::EnumWarpAdapter`).
  - It then builds with `--config patch.crates-io…` into `target\warp`. The default build is unpatched.

## 4. Method

**Machine.** Ryzen 5 7600X (6 cores, 12 threads), 63 GB RAM, RTX 3070, Windows 11 (NT 10.0.26300). The display is 2560×1440 at 125 % and 60 Hz. Rust 1.99.1; release profile `lto = "thin"`, `codegen-units = 4` (as the root package).

**Phases of `--measure`.** Both spikes run the same timeline:

| Phase | What is shown | Sampled | Reports |
|---|---|---|---|
| boot | — | process start until the first frame is presented and every noise tile has a frame | `first_paint_ms`, measured from process creation (`GetProcessTimes`) |
| streaming_tiles (10 s) | normal layout: static canvas at zoom 0.6 (217 boxes, about 6,700 label glyphs); four `noise` tiles at 12 px (about 6,900 visible cells) | 2–10 s | frame statistics; `private_mb_after_10s_streaming` at 10 s |
| tiles_only_full_grid (7 s) | four `noise` tiles, every cell of 4 × 200 × 60 drawn (about 22,800 non-blank cells) | 2–7 s | frame statistics |
| pan_zoom (6 s) | static PowerShell tiles; canvas panned along a Lissajous path while zoom swings 0.12 ↔ 1.4 every 2.5 s | 1–6 s | frame statistics |
| idle (12 s) | nothing changes; static shells | 1.5–11.5 s | `idle_cpu_percent`, frames presented |
| latency | tile 0 = `echo`, tiles 1–3 = streaming `noise` | 60 keys | keystroke → present |

**Definitions**

- **`frame_ms`.** CPU time on the UI thread per frame.
  - GPUI: its own `frame duration` measurement (`window.draw()` + `present()`), enabled with `ZED_MEASUREMENTS=1`. The spike redirects its stderr into a pipe to read it.
  - wgpu: from the start of `RedrawRequested` to the return of `queue.present()`, excluding time blocked in `get_current_texture`, which is reported separately as `acquire_wait`.
  - `process_cpu_ms_per_frame` is the whole process's CPU time (all threads, including driver threads) divided by frames.
- **`app_*_ms`.** The spike's own paint/prepare code, as a breakdown.
- **Keystroke → present.** `PostMessageW(WM_KEYDOWN)` to the spike's own window at t0. The key goes through the framework's key and char handling, our mapping, `ClientMsg::Input`, the service, `echo`, and back as a `Frame`. The end is the first frame that started after the echo arrived, at the return of its present call.
  - Sub-measures: time until our handler forwards the key (`key_handled`), until the echo frame arrives (`service_frame`), and from that frame to present.
  - Display scan-out is not included, for either stack.
- **Memory.** `PrivateUsage` and `WorkingSetSize` from `GetProcessMemoryInfo`.

**Caveats**

1. **The console session was locked during all runs**, because the agent ran unattended.
   - Windows still renders and presents, and `PrintWindow` screenshots show correct output. DWM, however, was not composing to a display.
   - The vsync sources (GPUI uses `DwmFlush`/compositor-clock waits; wgpu Mailbox pacing uses our own 60 Hz timer) may behave differently on an unlocked desktop. GPUI's latency *tail* (frame → present p95 of 35–50 ms) is the number most likely affected.
   - CPU-time results and GPUI's draw-before-dispatch cost do not depend on this.
   - **Re-run `--measure` once on an unlocked desktop before Phase 4.**
2. **`SendInput` cannot reach a locked desktop**, so keys were injected with `PostMessage` (§6).
3. **Background terminals.** One service instance hosted all runs. Each run's nine terminals were stopped at the end, and the instance was restarted before the final runs. Four `noise` terminals stream throughout every run, as background load.
4. **TUI baseline on the same machine.** The service-only keystroke → frame time from `copilot-canvas bench --terminals 3`, with no UI, is p50 4.5 ms, p95 5.0 ms, p99 8.7 ms (`tui-bench-3-terminals.json`).

## 5. Results

Raw JSON is in `docs/spike-results/` (`gpui-gpu-{1,2,3}`, `wgpu-gpu-{1,2,3}`, `gpui-warp-{1,2}`, `wgpu-warp-{1,2}`, plus the variants `gpui-gpu-uncached-1` and `wgpu-gpu-fifo-1`). Build figures are in `build-stats.json`.

### Hardware GPU (RTX 3070), median of 3 runs

| Measure | GPUI | wgpu + glyphon |
|---|---|---|
| First paint (process start → presented) | **292 ms** (279–315) | 461 ms (458–496) |
| Private memory: after GPU init / after 10 s of 4 streaming tiles / at end | **128 / 145 / 189 MB** | 135 / 181 / 205 MB |
| Working set after 10 s | **75 MB** | 131 MB |
| Pan + zoom, frame ms p50 / p95 (fps) | 2.37 / 6.33 (60) | **1.60 / 5.87** (59) |
| Pan + zoom, process CPU | 19.7 % (3.3 ms per frame) | **16.0 %** (2.7 ms per frame) |
| 4 streaming tiles (normal layout), frame ms p50 / p95 | 6.45 / 7.36 | **1.29 / 1.60** |
| 4 streaming tiles, process CPU | 43.6 % (7.3 ms per frame) | **12.7 %** (2.2 ms per frame) |
| Tiles only, 22,800 cells, frame ms p50 / p95 (fps) | 6.94 / 7.75 (56) | **1.92 / 2.27** (59) |
| Idle CPU over 10 s / frames presented | 0.16 % / 0 | **0.0 % / 0** |
| Keystroke → present, p50 / p95 | 34.7 / 50.9 ms | **9.9 / 11.7 ms** |
| … key handled by the app (p50) | 6.4 ms (GPUI draws the dirty window first) | **0.1 ms** |
| … key → echo frame arrives (p50) | 13.5 ms | **4.7 ms** (TUI bench: 4.5 ms) |
| … echo frame → present, p50 / p95 | 21.2 / 35.1 ms | **5.3 / 10.1 ms** |
| Release exe / dependency crates / clean release build | 11.3 MB / 444 / 129 s | **8.5 MB / 268 / 75 s** |

Ranges and variants:

- **GPUI without view caching** (`--uncached`): streaming tiles 7.06 ms p50, canvas paint 4.2 ms per frame. With caching it is 6.45 ms: the canvas paint is skipped but its primitives are still replayed.
- **wgpu with FIFO presentation:** frame times are the same, but keystroke → present is 33.3 ms p50. The UI thread waits about 15 ms per frame in `get_current_texture`, which delays the echo frame (16.7 ms to arrive).
- **Where the frame time goes, 4 streaming tiles:**
  - GPUI: our paint code 2.0 ms (canvas cached), GPUI scene, render and present about 4.4 ms.
  - wgpu: tile layout plus glyphon prepare 0.5 ms, canvas 0.45 ms, submit and present about 0.3 ms.
- **Baseline for size and build:** the canvas crates alone (`conn` + `TermView`) give a 0.48 MB exe, 186 crates and a 41 s build.

### Software rendering (WARP; proxy for a GPU-less Cloud PC), 2 runs each

| Measure | GPUI (patched to pick WARP) | wgpu (DX12 WARP) |
|---|---|---|
| First paint | **132–143 ms** | 490–528 ms |
| Private memory after 10 s / at end | **101 / 141 MB** | 144 / 192 MB |
| Pan + zoom fps / process CPU | 20.8 fps / 815 % | **28 fps / 455 %** |
| 4 streaming tiles fps / process CPU | **19.2 fps** / 850 % | 9.6 fps / 677 % |
| Tiles only (22,800 cells) fps | **25.7 fps** | 7.6 fps |
| Idle CPU | 0–0.16 % | 0 % |
| Keystroke → present, p50 / p95 | **117 / 164 ms** | 184 / 188–282 ms |

- **Both are unusable at this window size.** They peg 5–9 logical cores while terminals stream.
- **Where the time goes:**
  - GPUI: D3D11 WARP draws text 2–3× faster than glyphon on DX12 WARP.
  - wgpu with text skipped (`SPIKE_SKIP=text`) still only reaches 15–20 fps. DX12 WARP's per-frame cost is high.
  - Our frame code is small on both stacks; WARP's rasterizer threads are the cost.
- **Decision unchanged:** PLAN D4 already makes the home PC the primary target. On a GPU-less Cloud PC the TUI stays the default.
- **If a local Cloud PC GUI is wanted later**, wgpu allows the obvious fixes and GPUI does not:
  - redraw only damaged tiles with scissor rects and `Present1` dirty rectangles;
  - lower the frame rate for background tiles.

### PLAN §9 budgets (hardware)

| Budget | Target | GPUI | wgpu |
|---|---|---|---|
| Keystroke → present, local | p50 ≤ 8 ms, p99 ≤ 20 ms | ✗ 34.7 / p95 50.9 | ≈ 9.9 / p95 11.7. The client's part is 5.3 ms; the rest is the service path, the same as the TUI's. |
| Full redraw (canvas + 4 tiles) | ≤ 6 ms | ✗ about 6.5 | ✓ 1.3–1.9 |
| Damage-only frame | ≤ 2 ms | ✗ (always a full window) | ✓ (a full frame is already ≤ 2 ms; scissoring possible) |
| Panning | ≤ 16 ms per frame | ✓ p95 6.3 | ✓ p95 5.9 |
| Idle CPU | ≤ 1 % | ✓ 0.16 % | ✓ 0 % |
| Client memory (4 tiles, 300 boxes) | ≤ 250 MB | ✓ 145–189 | ✓ 181–205 (with `MemoryUsage`) |
| First paint | ≤ 1.5 s | ✓ 0.29 s | ✓ 0.46 s |

## 6. Qualitative notes

### API ergonomics

**GPUI**

- High-level and productive.
  - Entities and `Context`; `canvas()` for custom painting.
  - `paint_quad`: rounded corners, borders, dashed borders on boxes.
  - `paint_path` strokes with dash arrays (lyon).
- Text system:
  - DirectWrite with system fallback;
  - `shape_line(…, force_width)` for monospace grids;
  - `shape_line_by_hash` to avoid allocations on cache hits.
- Taffy layout, focus handles, actions and keybindings, multi-window, cached views, async tasks on the UI thread.
- The canvas plus tiles took about 900 lines, with no shaders.
- Friction:
  - The crate is a pre-1.0 snapshot (`gpui-pre`) of Zed's main branch. Renames are in flight: `on_scroll_wheel` → `on_scroll`, `ScrollWheelEvent` → `ScrollEvent`.
  - Documentation is thin; the examples and source are the reference.
  - There is no public frame-timing hook (the spike scrapes `ZED_MEASUREMENTS` from stderr) and no adapter selection.
  - Dashed borders do not render on hairline quads; the spike draws dash quads instead.

**wgpu + winit + glyphon**

- More code, all of it straightforward:
  - a 200-line instanced SDF pipeline (rounded rects, borders, dashed segments, per-instance clip);
  - glyphon `TextArea`s (scale per area for zoomed labels, bounds for clipping);
  - a winit `ApplicationHandler`.
- We control presentation, pacing, adapter choice and memory hints. Each of these decided a measured result above.
- glyphon has no retained geometry: it re-prepares every visible glyph each frame. At about 0.06 µs per glyph that is the cheapest path measured.

### Text quality

- **GPUI:** DirectWrite rasterization; crisper at small canvas label sizes.
- **glyphon (swash, grayscale, unhinted):** slightly softer and lighter.
- **Both render:**
  - Cascadia Mono, including bold through its variable weight axis;
  - CJK through fallback, two cells wide and centred;
  - colour emoji (Segoe UI Emoji), braille spinners, box drawing and `✓`;
  - dim and inverse.
- Screenshots: `gpui-window.png`, `wgpu-window.png`, `*-labels-zoom.png`.

### IME

- **GPUI:**
  - `EntityInputHandler` receives `replace_and_mark_text_in_range` (preedit) and `replace_text_in_range` (commit) from its WM_IME_COMPOSITION handling.
  - `bounds_for_range` places the candidate window at the terminal cursor.
- **winit:** `set_ime_allowed(true)`, `Ime::Preedit`/`Ime::Commit`, and `set_ime_cursor_area` (updated when the cursor cell moves).
- **Verified:** with `--ime-demo`, both spikes drew the preedit "にほんご" inline (underlined, on a tinted background). The commit "echo 日本語 ✓ 🙂" reached PowerShell and was echoed with correct widths. See `*-ime-pre.png` and `*-ime-commit.png`.
- **Not verified:** the synthetic events enter at the framework's handler level, not through a real IME. A real Japanese or Chinese IME session is still to be tested by hand. Both stacks use IMM32, not TSF.

### AltGr

- **GPUI:**
  - Marks key-downs whose character changes with modifiers as `prefer_character_input`. The spike then leaves them to `WM_CHAR` → `replace_text_in_range`.
  - `--altgr-demo` types "echo " with WM_KEYDOWN, then posts the WM_CHARs a German layout produces for AltGr+Q/E/7/0. PowerShell showed `echo @€{}` (`gpui-altgr.png`).
  - **Not exercised:** the key-down classification itself on a real AltGr layout.
- **winit:**
  - On layouts with AltGr it removes Ctrl and Alt from the modifiers while right Alt is held, and delivers the text.
  - The spike's rule ("Ctrl+Alt with printable text is text") covers the other case. It is unit-tested.
  - The `--altgr-demo` simulation on this machine's US layout (key state Ctrl+Alt+RAlt, WM_KEYDOWN, WM_CHAR) produced Ctrl+Alt chords (`^Q^E^_`, `wgpu-altgr.png`). That is correct winit behaviour for a layout without AltGr, so it does not validate AltGr.
- **Left to do:**
  - `SendInput` was not feasible (locked session), and the session language could not be switched (`ActivateKeyboardLayout` only acts for the foreground process).
  - Check AltGr on a German layout by hand for whichever stack is used.
  - Add the PLAN Phase 2 key-table tests.

### Multi-window

- **GPUI:** built in. `cx.open_window`; entities are shared across windows.
- **winit + wgpu:** also supported. One device, one surface per window, events routed by `WindowId`; slightly more code.

### Windows maturity

- **GPUI:** built and ran correctly on the first attempt. It uses D3D11 + DirectComposition + DirectWrite with precompiled HLSL.
- **Concerns:**
  - its latency structure (§1);
  - the missing adapter switch;
  - frame pacing tied to `DwmFlush` and the compositor clock;
  - the pre-1.0 snapshot crate.
- **wgpu and winit:** mature and widely used on Windows. winit 0.31 (in beta) will bring API changes.

### Build friction

- **GPUI:** none in practice. Its build script found `fxc.exe` through the Windows SDK registry key (no `GPUI_FXC_PATH` needed), and `embed-resource` found `rc.exe`. However, it adds 258 crates over the baseline, a 129 s clean release build and 24 s incremental builds.
- **wgpu:** 82 extra crates, a 75 s clean build and 17 s incremental builds.
- **Both** needed the shared `windows-sys` 0.61 feature set only.

## 7. How much of `canvas-core` the spikes reuse

**Used directly by both spikes, identically:**

- `canvas_core::conn::try_connect`: named pipe, HMAC handshake, reader and writer threads, crossbeam channels.
- `canvas_termview::TermView`: applies `Frame`s.
- `canvas_common::protocol`: `ClientMsg`, `ServiceMsg`, `Frame`, `Run`, `Input`, `Key`, `LaunchKind`, `MOD_*`, `F_*`, `COLOR_*`.
- `canvas_common::paths::Instance`.

The spikes' own shared code (scene, terminal cell layout, key mapping, service client, measurement: 1,468 lines) was written once and used by both stacks. The same split carries into the GUI.

**What a GUI could reuse from `canvas-core` today** (about 10,100 lines), whichever stack is chosen:

| Status | Files | Lines | Share |
|---|---|---|---|
| UI-free now | `actions.rs`, `conn.rs`, `clipboard.rs`, `factory_client.rs`, `passive.rs` | about 2,040 | 20 % |
| Needs a trivial type change (`side.rs` and `status.rs` use `ratatui::style::Color`; `App` geometry is `ratatui::layout::Rect` in character cells) | `app.rs`, `side.rs`, `status.rs` | about 2,970 | 29 % |
| Input and dialog dispatch, written against crossterm `KeyEvent`/`MouseEvent` (`input::InputEvent` still wraps a crossterm `Event`) | `input.rs`, `events.rs`, `dialogs.rs`, `modals.rs` | about 2,170 | 21 % |
| TUI drawing only | `ui.rs`, `ui_canvas.rs`, `ui_modals.rs`, `ui_side.rs`, `paint.rs`, `links.rs` | about 2,900 | 29 % |

So about half of `canvas-core` is reusable as-is or nearly. The dispatch part becomes reusable once Phase 0's `ViewModel` and a platform-neutral `InputEvent` exist; neither does yet. That work is independent of the GPU stack:

- **GPUI** would host `App` in an `Entity`.
- **wgpu/winit** would host it in the `ApplicationHandler`.

The spike adds no reason to prefer either stack here.

## 8. Follow-ups for Phase 4

1. Re-run `--measure` for both stacks on an unlocked desktop (the GPUI latency tail and vsync pacing). If GPUI's p50 latency stays above 20 ms, close the GPUI option.
2. Week 1: egui chrome over the wgpu canvas (§1 condition), measured with the same driver.
3. Keep in the client:
   - Mailbox presentation with refresh pacing (never block the UI thread in acquire);
   - `MemoryHints::MemoryUsage`;
   - the per-cell text segmentation of `term.rs`;
   - shape caches keyed by content;
   - the canvas label "shape once, scale at paint" scheme.
4. Pipeline creation off the first frame; a font fallback list instead of scanning the system font database.
5. Manual IME (Japanese, Chinese) and AltGr (German) checks; protocol-8 graphemes when Phase 2 lands. The spikes still use protocol-7 `Run`s.
6. Cloud PC: keep the TUI as the default. Revisit damage-only presentation on WARP only if a local GUI there is requested.
