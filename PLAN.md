# Copilot Canvas roadmap: layers, remote mode, libghostty and a GPU client

Status: approved direction, 2026-10-27. Owner: Thomas. Baseline: `main` at 44fc899.

This plan turns Copilot Canvas into three layers so that:

- agents run on the Cloud PC while the app runs on the home PC (**remote mode**, over Dev Tunnels);
- terminals use **libghostty-vt**;
- a **native GPU client** runs on the home PC's real GPU;
- every feature of today's terminal UI keeps working (**parity**, §8) before anything changes default.

It supersedes `docs/GPU-CANVAS-PLAN.md`.

## 0. Implementation status (2026-10-28)

Published at https://github.com/TomND/copilot-canvas (CI: GitHub Actions, a cached
pinned-Ghostty job and the test matrix).

All phases are implemented on `main` of the new repository. Besides GitHub
Copilot CLI, the canvas now also runs **OpenCode** and **Claude Code** sessions
(agent providers in `canvas-agents/src/agents`); it defaults to the first
installed agent (OpenCode on the development machine, which has no Copilot).

| Phase | State | Verified by |
|---|---|---|
| 0 Layer split | Done: workspace crates (§3), `Emulator` trait, `Backend` RPC trait (local and remote), `InputEvent`, `RenderMode`/`CanvasModel` view model, golden corpus (13 entries, both engines) | `cargo test --workspace`; `corpus check`; `selftest` (TUI, 27 steps) |
| 1 Remote mode | Done: protocol 8, TCP + zstd, heartbeat, reconnect (backoff capped at 1.5 s, see D5), input acks, offline blocking, per-tile coalescing, backend-owned document with patches, pairing (DPAPI), `CopilotCanvas-Tunnel` manifest, managed `devtunnel connect`, §6 behaviours | `selftest --remote` `loopback`, `faults`, `paired`; backend contract tests; 20-minute soaks (docs/soak) |
| 2 libghostty-vt | Done and default: pinned Ghostty `521cf2c5`, Zig 0.17.0, `cargo xtask build-ghostty`, vendored headers with checksums; corpus differences reviewed (corpus/reviewed-differences.md) | `bench --terminals 100`: equal CPU, about 58 % less memory per terminal, keystroke p50 ~1.1 ms |
| 3 GPU spike | Done: wgpu 30 + winit + glyphon chosen over GPUI (docs/GPU-SPIKE.md) | measured on the RTX 3070 and WARP |
| 4 GUI MVP | Done: `copilot-canvas-gui.exe` | `selftest --client gui` (+ `--remote loopback`) |
| 5 Parity | Done: all 33 rows of §8 plus images, taskbar/flash/toasts, drag and drop, several windows (docs/PARITY.md) | `selftest --client gui --parity` (38 checks) |
| 6 Hardening | Done: damage frames, device-lost and DPI recovery, window placement, UI Automation (AccessKit), crash log, GUI stress/perf/burn probes | `selftest --client gui --hardening`; §9 budgets all met (docs/GUI.md) |
| 7 Rollout | Done except the time-gated step: `client` setting (`tui`, `gui`, `auto`; `copilot-canvas config client …`), `--tui`/`--gui`, **O** opens the same canvas in the other client | `selftest` step "O opens this canvas in the other client" (TUI and GUI) |

Not done in this session, because they need time or a second machine:

- **Dogfood week, then switch the default client** (Phase 7). The default is
  still `tui`; after the week, `copilot-canvas config client gui` (or `auto`,
  which keeps the TUI over remote desktop and SSH as D4 asks).
- **A day on a real Dev Tunnel** (Phase 1 exit criterion): the devtunnel CLI is
  installed but signed out here; everything was tested with a stand-in that
  forwards a port (docs/REMOTE.md has the manual checklist and commands).
- **The 48 h remote soak** (Phase 6): 20-minute soaks passed for both clients;
  the 48 h run with the GUI client started on 2026-10-28 11:46 (report:
  `%TEMP%\cc-soak48\soak-gui-48h.json`; after 2 h, 0 keystrokes lost or
  doubled, reconnects ≤ 1.54 s, flat memory and handles).
- **Real-Copilot probes**: Copilot CLI is not installed on the development
  machine; the Copilot provider keeps the original command lines (unit-tested)
  and `selftest agent --agent copilot` runs them where Copilot is installed.
  OpenCode was verified end to end through the canvas with a free model
  (`selftest agent --agent opencode --prompt`: re-keying, status, needs-you,
  subagents, wheel, push detection, resume, passive runs; docs/AGENTS.md);
  Claude Code was launched and stopped through the canvas, never prompted.

## 1. Goals and non-goals

### Goals

1. **Remote mode.** Agents, their terminals, history, automations and passive sessions run on the Cloud PC. The canvas runs on the home PC and survives network drops: it reconnects and resyncs; the agents never notice.
2. **Clean layers** (§3). A generic terminal host, a terminal view, and the Copilot canvas built on both, with explicit APIs between them.
3. **Correct terminals.** Ghostty's VT core for:
   - graphemes and widths;
   - the Kitty keyboard protocol;
   - Kitty graphics;
   - modern SGR and OSC sequences.
4. **GPU client.** Pixel-smooth canvas, real fonts and inline images, rendered on the home PC's GPU.
5. **Parity** with everything the TUI does today (§8), verified by automated tests (§10).
6. **No responsiveness regression** (§9).

### Non-goals

- Ghostty's own app runtime or renderer (no Windows support; not needed).
- Exposing anything to the internet without Dev Tunnels authentication.
- Dropping the TUI. It stays a first-class client: local, over SSH, and in remote mode.

## 2. Why remote mode is safe to build on Dev Tunnels

Earlier trouble with VS Code's remote Agents came mostly from the Agent Host, not the tunnel transport. From Thomas's investigation sessions (Sep–Oct 2026):

- duplicate host trees with 1,207 child processes;
- a runtime growing to 6.5 GB;
- MCP and managed-settings timeouts (VS Code #342123, #342161, #340966).

The one tunnel-specific problem was 401 errors with Microsoft-account authentication; GitHub authentication avoided it.

Our design is also less exposed to drops:

- **The service on the Cloud PC owns all state:** terminals, history index, bus, automations and the canvas document.
- **A client holds no state.** On reconnect it receives a full snapshot, as every client already does on connect today.
- **A drop costs a banner and a second**, not a stuck session. The tunnel host itself is a manifest-managed automation, so it restarts on failure and the Doctor watches it.

## 3. Layers

```
 HOME PC                                         CLOUD PC (agents run here)
┌──────────────────────────────┐               ┌───────────────────────────────────────────┐
│ Canvas UI (L3 front end)     │               │ copilot-canvas service (one process)       │
│  GPU client  or  TUI client  │   Dev Tunnel  │  L1 terminal host                           │
│  └ Terminal view (L2)        │◀════════════▶ │    ConPTY · Emulator (libghostty-vt)        │
│ local: clipboard, toasts,    │  TLS + auth   │    sessions survive clients · frames · input│
│ taskbar, link opening        │  heartbeat,   │  L3 canvas backend ("agent supervisor")     │
└──────────────────────────────┘  reconnect    │    history index · canvas doc (owner)       │
                                               │    bus + passive runner · automations       │
         same clients also work locally ──────▶│    Task Scheduler · copilot config · VS Code │
           over the named pipe                 │    file browse · git · worktrees · clipboard │
                                               └───────────────────────────────────────────┘
```

### L1: terminal host (`canvas-host`)

- Generic: ConPTY sessions, the `Emulator` trait (libghostty-vt by default, alacritty as fallback), frame diffs, input encoding, scrollback, selection text, resize, the process priority manager, graceful stop.
- Transports: named pipe (local) and TCP on loopback (remote, behind the tunnel). Both carry the same framed protocol.
- No Copilot knowledge. A plain remote terminal app could ship on it alone.

### L2: terminal view (`canvas-termview`)

- Draws L1 frames and maps input events to L1 input. It provides:
  - a GPU widget (graphemes, cursor shapes, underline styles, selection, images, links);
  - a TUI adapter (today's terminal drawing).
- No Copilot knowledge either.

### L3: Copilot canvas

- **Backend** (`canvas-agents`, inside the service process): everything that has to run where the agents are:
  - history index, status and subagents;
  - canvas document storage and broadcast;
  - bus, passive runner and automation watcher;
  - Copilot config CLI, Task Scheduler, VS Code storage, migration;
  - folder browsing, git detection, worktrees;
  - forking, the MCP server list, the Cloud PC clipboard.
- **Core** (`canvas-core`): the platform-free app state machine (today's `App` minus drawing), actions, dialogs as data, `ViewModel`, `InputEvent`.
- **Front ends:** `canvas-tui` (ratatui) and `canvas-gui` (GPU), each drawing the `ViewModel` and embedding L2 for terminals.

One repository, one Cargo workspace. Binaries: `copilot-canvas.exe` (service, CLI, TUI) and `copilot-canvas-gui.exe`.

**Process rule:** L1 and the L3 backend share the service process with an internal API boundary (a Rust trait). They only become separate processes if a real need appears.

## 4. Key decisions

### D1. Engine: libghostty-vt behind `trait Emulator` (L1)

- The trait covers:
  - feed bytes; resize;
  - render snapshot plus dirty rows;
  - scrollback viewport; selection text;
  - modes (mouse tracking, bracketed paste, alt screen, application cursor, Kitty keyboard flags);
  - callbacks for title, OSC 9;4 progress, bell, OSC 52, OSC 8, OSC 777;
  - key, mouse and paste encoding (replaces the hand encoder in `service/keys.rs`).
- Implementations:
  - `GhosttyEmulator` (the `libghostty-vt` crate over `ghostty/vt.h`; `!Send`, so one per terminal thread);
  - `AlacrittyEmulator` (today's code).
- Setting `terminal.engine = ghostty | alacritty`, with a one-time fallback if Ghostty fails before the shell starts.
- Build: pin the Ghostty commit and Zig version, link `ghostty-vt-static.lib` statically, vendor headers and bindings with checksums, add an `xtask build-ghostty` step. Engine updates are one reviewed change together with the golden corpus (§10).

### D2. Protocol 8

The terminal channel (L1):

- `Cell` gains grapheme clusters, underline style and colour, strikethrough, overline, a hyperlink id, and an engine-provided wide/spacer flag.
- `Frame` gains cursor shape and blink, and a mode bitset.
- New `Image` and `Placement` messages carry Kitty graphics (deduplicated per client).
- Clients advertise capabilities: the TUI renders no images unless the host supports Sixel; the GUI renders them.

The canvas channel (L3):

- `Doc` snapshot plus `DocPatch` (the backend owns `canvas.json`).
- RPCs for every local read the client does today (§6).
- Bus and status snapshots as today.

Ordering rules are unchanged: append-only enums, positional structs, `PROTOCOL = 8`. Protocol-7 TUI clients are served the old cell shape during the transition.

### D3. GPU UI stack: GPUI or wgpu with glyphon, after a 1-week spike

**GPUI**
- For: layout, text, IME and windows built in; Zed's terminal panel shows terminal grids work; Paneflow ships libghostty-vt on GPUI on Windows.
- Against: pre-1.0; Windows less mature.

**wgpu + winit + glyphon**
- For: full control, small dependencies.
- Against: we write layout, IME, menus and lists ourselves.

Spike criteria:

- four 200×60 streaming tiles;
- 300 boxes, 30 groups and 3,000 subagents while panning;
- IME and AltGr;
- first paint, memory, binary size;
- how much of `canvas-core` the spike reuses.

Default if results are close: GPUI.

**Decision (Phase 3 spike, 2026-10-27): wgpu + winit + glyphon.** The results were not close where it matters most:

- **Four streaming tiles:** 1.3 ms per frame against GPUI's 6.5 ms, and 13 % CPU against 44 %.
- **Keystroke → present:** p50 9.9 ms against 34.7 ms.

GPUI was better on first paint (0.29 s against 0.46 s) and memory (145 MB against 181 MB); both are within budget. Conditions:

- egui (or a small widget layer) for the chrome, prototyped in Phase 4 week 1;
- Mailbox presentation with our own pacing;
- `MemoryHints::MemoryUsage`.

Details: `docs/GPU-SPIKE.md`.

### D4. Where the GPU client runs

- **The home PC, with a real GPU, in remote mode.** This is the primary target.
- Locally on the Cloud PC (no GPU: Direct3D falls back to WARP, a software rasterizer, and RDP encodes the frames) is secondary and must not be worse than the TUI there. Otherwise the TUI stays the default on that machine.
- Either way, rendering is damage-driven: no continuous 60 fps; animations ≤ 15 fps and clipped.

### D5. Remote transport and security

- **Service:** `remote.listen = 127.0.0.1:<port>`. It is off until you pair a client.
- **Tunnel:** a persistent, private Dev Tunnel (`devtunnel create copilot-canvas`, plus a port). It is hosted by the automation manifest `CopilotCanvas-Tunnel` (logon and hourly, restart on failure), and only the tunnel owner can connect (Microsoft or GitHub sign-in).
- **App-level authentication:** `copilot-canvas remote pair` on the Cloud PC prints a one-time pairing code. The home client exchanges it for a long-lived client secret, stored with DPAPI on both sides; the secret can be revoked (`remote clients`, `remote revoke`). Each connection runs an HMAC challenge-response with the secret, the same scheme as the pipe's per-start token.
- **Home client:** `copilot-canvas --remote <tunnel>` (and the GUI equivalent) manages `devtunnel connect` as a child process, then connects to the forwarded localhost port.
- **Liveness:**
  - heartbeat every 2 s; a connection is dead after 6 s of silence;
  - reconnect with backoff 0.5 s → 1.5 s (Phase 6; was 5 s, see `docs/REMOTE.md`);
  - an "offline, reconnecting" banner;
  - **terminal input is blocked while offline**, never queued, so nothing is typed into a changed state;
  - on reconnect, a full snapshot and full frames for visible tiles.
- **Bandwidth:**
  - frame coalescing per tile (≤ 60 fps for the focused tile, ≤ 10 fps for background tiles);
  - optional zstd compression on the remote transport;
  - images sent once per id.

## 5. Phases

Order changed from the earlier draft: remote mode with the existing TUI comes first, because it delivers value without any GPU work.

| Phase | Scope | Estimate |
|---|---|---|
| 0 | Layer split (workspace, `Emulator` trait, `ViewModel`, `InputEvent`, backend API), golden corpus | 1 week |
| 1 | Remote mode with the TUI client at home | 2–3 weeks |
| 2 | libghostty-vt engine | 1.5–2 weeks |
| 3 | GPU spike and decision (home GPU; Cloud PC check) | 1 week |
| 4 | GUI MVP (local and remote) | 2–3 weeks |
| 5 | Feature parity (§8) | 3–4 weeks |
| 6 | Hardening and performance | 1–2 weeks |
| 7 | Rollout | ≈1 week |

**Total: about 12–17 weeks.** Phases 1 and 2 each ship on their own.

Every phase ends green: unit tests, clippy, the TUI selftest, the real-Copilot probes it touches, and its own tests.

### Phase 0: Layer split (1 week)

- Create the workspace crates (§3). Move state and logic out of `client/ui*.rs` and `paint.rs` into `canvas-core`. The TUI stays byte-identical (existing selftest and golden screens).
- Define the backend API as a trait implemented in-process, so the TUI keeps working locally through it. List every local read today (§6) and route it through the trait.
- Port the alacritty code behind `Emulator` with no behaviour change.
- **Golden corpus:** recorded PTY byte streams with expected cell grids. Sources:
  - real Copilot sessions: prompts, tool output, diffs, tab bar, mouse mode, permission prompt;
  - PowerShell, git, vim;
  - emoji, CJK, ZWJ sequences;
  - OSC 8, 52, 9;4, 777 and bell.

### Phase 1: Remote mode (2–3 weeks)

- TCP transport in the service; framing shared with the pipe; heartbeat, backoff and resync; offline banner and input blocking (D5).
- Pairing and secrets (`remote pair`, `clients`, `revoke`); the `CopilotCanvas-Tunnel` manifest; `devtunnel connect` management in the client; `copilot-canvas --remote`.
- The canvas document moves to the backend: the backend is the single writer and every client applies the patches it broadcasts. Two clients (home and Cloud PC) can be open at once; edits are serialized by the backend.
- Every client-local read becomes a backend RPC (§6).
- Local-versus-remote behaviours (§6, "where it happens"): clipboard, image paste, links, folders, notifications.
- Tests: the selftest runs with `--remote loopback`, which uses the TCP transport without a tunnel. A fault-injection probe drops the TCP connection mid-typing and mid-frame and checks the resync and that no input is lost or duplicated. The real-Copilot probes run over loopback remote.
- Exit criteria: the TUI at home through a real tunnel passes a manual checklist over a day, and a forced drop during an active turn recovers in under 5 s with the session unaffected.

### Phase 2: libghostty-vt (1.5–2 weeks)

- Pinned build pipeline and CI job; `GhosttyEmulator`; input encoding moves to the engine (keeping a table test for every key the TUI forwards, including Shift+Enter, AltGr and dead keys); protocol-8 cells.
- The TUI renders graphemes and underline styles where the host can. Its width-artifact healing stays in the TUI only.
- Exit criteria:
  - the golden corpus matches, or differences are reviewed;
  - `bench --terminals 100` CPU and memory at or below alacritty;
  - `selftest copilot` passes;
  - typing latency p99 unchanged.
- Then switch the default engine to Ghostty.

### Phase 3: GPU spike (1 week)

- Two thin prototypes (GPUI; wgpu with glyphon), each with a canvas of 300 boxes and one live tile over **remote** protocol 8 from the Cloud PC, plus IME input.
- Measure §9 on the home PC and on the Cloud PC.
- Record the D3 decision.

### Phase 4: GUI MVP (2–3 weeks)

- Window chrome, toolbar, activity bar, sidebar, canvas, terminal area; single and tiled layouts; DPI; fonts (Cascadia Mono, with fallbacks for emoji, CJK and symbols).
- Canvas: boxes, groups, edges (subagent, subsession dashed, loopback arc), status styles, halo, continuous zoom, drag, rubber-band select, group create and resize, menus.
- Terminals through L2: graphemes, cursor shapes, underline styles, user-message tint, selection, scrollback rules, typing, paste, copy.
- Local and remote connection, reconnect banner, the upgrade flow.

### Phase 5: Parity (3–4 weeks)

Work through every row of §8 in this order:

1. sessions;
2. status;
3. terminal behaviours;
4. dialogs;
5. sidebar views;
6. passive sessions;
7. automations;
8. VS Code sync and migration;
9. settings;
10. help.

Each row gets a GUI test and a ✓.

New in the GUI:

- inline images, including pasted screenshots;
- ITaskbarList3 progress, FlashWindowEx and toasts;
- file drag and drop (local files are uploaded in remote mode);
- several windows.

### Phase 6: Hardening (1–2 weeks)

- GUI ports of the stress, perf and burn probes.
- Crash log; GPU device-lost recovery; window placement.
- UI Automation tree (used by tests).
- Long-run remote soak: home client connected for 48 h with automatic drop injection.

Status (Phase 6, 2026-10-28): `bench --client gui --stress | --perf-canvas | --burn`,
damage frames (only changed terminal tiles are repainted and uploaded),
GPU device-lost and surface recovery, WARP fallback, DPI changes, saved
window placement, an AccessKit UI Automation tree, crash-log details and
`selftest --client gui --hardening` are in; numbers against §9 in
`docs/GUI.md` §4. `selftest --remote soak` ran 20 minutes per client (TUI,
GUI) with a fault every 30 s (`docs/REMOTE.md`); the 48 h run is still to do.

### Phase 7: Rollout (≈1 week)

- `client = gui | tui` setting.
- A dogfood week with the TUI one keystroke away (same service, same document).
- Switch the default after a week with no open parity bugs. The TUI stays supported.

## 6. Local reads that move behind the backend (remote mode)

| Today (client reads it directly) | Remote mode | Where it happens |
|---|---|---|
| `canvas.json` load and save | Backend owns it; snapshot plus patches | Cloud PC |
| Copilot automations (`data.db`), Task Scheduler, manifests, run, enable, disable | Backend RPC | Cloud PC |
| `copilot plugin/mcp/skill/instruction/config` lists and changes; plugin install in a terminal | Backend RPC; the install terminal is a normal host session | Cloud PC |
| VS Code session sync, migration preview and apply | Backend RPC (VS Code storage on the Cloud PC) | Cloud PC |
| New session: folder browser, Recent, Git detection, branches, worktree creation | Backend file-browse and git RPCs | Cloud PC |
| Group folders (create `ai-docs`, `human-outputs`, instructions) | Backend | Cloud PC |
| Fork-on-subsession, MCP server list for the picker | Backend | Cloud PC |
| Passive session files (rules, memory, log), opening them | Backend reads; shown in a viewer or opened via VS Code tunnel URI | Cloud PC files |
| Automation outputs, manifests, logs (open) | Text viewer in the canvas; "Open in VS Code" via `vscode://vscode-remote/tunnel+<name>/<path>` | Cloud PC files |
| Links in terminals: URLs | Open on the home PC | Home |
| Links in terminals: Windows paths and folders | VS Code tunnel URI (files) or the text viewer; "Copy path" for folders | Cloud PC files |
| Text copy and paste, OSC 52 | Home clipboard | Home |
| Image paste (Alt+V / Ctrl+V) | Client reads the home clipboard image, sends it; the backend puts it on the Cloud PC clipboard, then sends the paste key so Copilot attaches it | Both |
| Bell, OSC 777, taskbar progress | Home (toast, taskbar) | Home |
| Crash log | Each side keeps its own | Both |

## 7. Interfaces to build first (Phase 0)

- **`Emulator`** (L1), shaped around Ghostty's render state and dirty rows.
- **`Backend`** (L3): one trait with every RPC in §6, plus document patches and subscriptions. Two implementations: in-process (local) and a protocol client (remote). The UI never touches the file system, Copilot, Task Scheduler or VS Code storage directly.
- **`ViewModel`** (core), a pure function of app state:

  ```
  ViewModel {
      toolbar, activity, sidebar_rows, canvas { boxes, edges, groups, labels, pulses },
      panel { lines, spans, buttons }, tiles [ { id, title, frame_ref } ], menus, modals, notices, banner,
  }
  ```

- **`InputEvent`** (core): platform-neutral keys (text and physical key), mouse, wheel and paste; one dispatch for both front ends. `dialogs::Action` stays the action vocabulary.

## 8. Parity matrix (all ✓ before any default changes)

Layer: H = terminal host, B = canvas backend, C = core, V = terminal view, G = GPU front end. "Remote" notes behaviour in remote mode.

Status (Phase 5, 2026-10-28): every row ✓ in the GUI, each with a named check in `copilot-canvas selftest --client gui --parity`; the completed matrix and the GUI-only additions are in `docs/PARITY.md`.

| # | Feature (today) | Layers | GUI work | Remote | Test |
|---|---|---|---|---|---|
| 1 | Canvas: drag boxes (subagents follow), pan (drag, middle-drag, wheel, Shift+wheel, Ctrl+arrows), zoom (Alt/Ctrl+wheel, toolbar, `+`/`-`, `F` fit, `C`/Home center), double-click creates, right-click menus | C G | continuous zoom; pixel hit testing | doc patches | gui drag, pan, zoom |
| 2 | Status (working, needs you, done · review, idle, stopped, failed, open elsewhere), OSC 9;4, background subagents, Esc not counted as done | H B C | styles | status stream | status probe |
| 3 | Attention borders and tints; active-terminal halo (heavy while typing) | C G | strokes | — | golden render |
| 4 | Colour-blind-safe palette; distinct icon and border per state | C | theme tokens | — | golden render |
| 5 | Subagents from history | B C | — | — | stress probe |
| 6 | Subsessions: `S`, fork (default) or fresh, dotted connector; spawned-subsession detection | B C | dashed edges | backend fork | fork, spawn probes |
| 7 | Looping sessions: loopback arc with a moving dot, `↻ every`, sidebar ↻, pulsing ↻ when zoomed out | B C G | arc animation ≤ 15 fps | — | loop probe |
| 8 | MCP picker with presets (Ctrl+S, ←/→, Del), per session and resume | B C G | checklist | server list from backend | mcp probe |
| 9 | Groups: select, `G`, move by title, resize ◢, nested, geometric membership, folder chips, launch defaults | B C G | — | folders on Cloud PC | selftest step |
| 10 | Sidebar tree, `/` filter, click flies to the box, group rows tile terminals | C G | virtualized list | — | selftest step |
| 11 | Terminals: ConPTY, single or 4 tiles (`T`), survive client exit | H V G | tiles, splitter | survive drops | selftest, detach probe |
| 12 | Wheel rules (mouse mode, PgUp/PgDn when mouse off, scrollback, Shift forces scrollback) | H C V | pixel wheel accumulation | — | scroll probe |
| 13 | User-message tint (navy background, ▎ bar) | C V | — | — | golden render |
| 14 | Copy and paste: drag-select (soft-wrap rejoin), copy on release, Ctrl+C with selection, Ctrl+Shift+C, Ctrl+V, pane menu | H C V | native clipboard | home clipboard | copy probe |
| 15 | Image paste (Alt+V / Ctrl+V) | B C G | direct Ctrl+V; show the image | forwarded (§6) | image probe |
| 16 | Click forwarding in Copilot's mouse mode; drags still select | H C | — | — | click probe |
| 17 | Title in the pane header, taskbar state mirror, bell, OSC 777 | H C G | ITaskbarList3, toasts | home side | progress probe |
| 18 | Links: URLs, OSC 8, Windows paths; Ctrl+click in terminals; browser, Explorer, VS Code at line; nothing executed | C V G | hover underline | §6 mapping | links probe |
| 19 | New session dialog: name, folder (type, Browse…, Recent ▾), Git detection, worktree, first prompt, MCP row | B C G | folder browser on the backend | backend browse | selftest step |
| 20 | Import (filter, multi-select, grouping); VS Code sync (+ Ctrl+Z, auto sync) | B C G | — | backend | vscode probe |
| 21 | Activity bar views (Alt+1…8) | C G | — | — | selftest steps |
| 22 | Copilot defaults, group overrides, `copilot config` list and edit | B C G | — | backend | selftest step |
| 23 | Plugins, MCP, skills: list, toggle, remove (confirmed), add, install in a terminal; secrets hidden | B C G | — | backend | selftest step |
| 24 | Migrate from VS Code (preview, tick, backup, redacted log) | B C G | — | backend | migrate test |
| 25 | Passive sessions: templates, panel, dashed ⟲ boxes, ✉ badge, event pulses, idle-only messages, mute, rollover, budgets | B C G | pulse dots along edges | — | passive probe |
| 26 | Automations: families, failure pulse, toolbar badge, info and health page, run, enable, ask doctor, outputs; doctor auto-created | B C G | coloured run strip; links | outputs via viewer or VS Code | factory probes |
| 27 | Copilot automations (`data.db`) and session schedules | B C | — | backend | selftest step |
| 28 | Idle auto-stop, priority manager, upgrade with resume, protocol-mismatch tolerance, crash log | H B C | crash log | upgrade over remote | upgrade probe |
| 29 | Keyboard map, F6 / Ctrl+] to leave a terminal, Q / Ctrl+Q quit dialog (detach or stop all), `?` help | C G | — | quit = disconnect | selftest |
| 30 | `--demo` mode | C | — | — | GUI selftest |
| 31 | `--perf` overlay | C G | GPU timings, round-trip time | RTT shown | perf probe |
| 32 | CLI (service, doctor, bench, events, emit, send, status, followup, automations) | H B | unchanged | runs on the Cloud PC | existing tests |
| 33 | **New:** remote mode (pair, reconnect, resync, offline input blocking) | H B C | banner | — | fault-injection probe |

## 9. Performance budgets

| Measure | Today (TUI, local) | Target |
|---|---|---|
| Keystroke to screen, local | p50 1.8 ms, p99 9.4 ms (to the frame) | GUI p50 ≤ 8 ms, p99 ≤ 20 ms to present |
| Keystroke to screen, remote | — | network RTT + ≤ 10 ms |
| Reconnect after a drop | — | ≤ 2 s after the network returns; full resync ≤ 500 ms |
| Remote bandwidth, 4 busy tiles | — | ≤ 200 KB/s (coalesced, compressed) |
| 100 sessions + 1,200 subagents, full redraw | p50 2.5–3.2 ms CPU | GUI ≤ 6 ms full frame, damage ≤ 2 ms |
| Panning 94 sessions | ≈2 ms input to screen | ≤ 16 ms per frame while dragging |
| Idle CPU, 10 live sessions | ~0 % | ≤ 1 % (no continuous rendering) |
| Animations | text cells | ≤ 15 fps, clipped; ≤ 2 % CPU |
| Client memory | small | GUI ≤ 250 MB (4 tiles, 300 boxes) |
| First paint | 0.4–1 s including service start | ≤ 1.5 s (remote: ≤ 1.5 s after connect) |
| Service, 100 streaming terminals | 14.5 % of one core, ~5 MB per terminal | libghostty ≤ alacritty |

## 10. Testing

- **Engine:** golden PTY corpus compared cell by cell for both engines; fuzz `vt_write`; key-encoding tables.
- **Core:** existing unit tests move with the code; `ViewModel` snapshot tests for scripted states (done: `crates/canvas-core/tests/view_snapshots.rs`, both render modes, `UPDATE_SNAPSHOTS=1` to accept).
- **Backend:** RPC contract tests run against both implementations (in-process and remote over loopback).
- **TUI:** existing `selftest`, golden screens and probes, run both locally and with `--remote loopback`.
- **Remote:** fault injection (drop the connection, delay, reorder at the frame boundary), resync assertions, a 48 h soak in Phase 6.
- **GUI:** `--test-hooks` gives an input-injection pipe, a UI Automation tree and render-to-PNG. A port of the selftest's 26 steps plus tolerant image diffs (done: `selftest --client gui` and `selftest --client gui --golden`, goldens in `tests/golden/gui/`); real-Copilot probes (passive, factory, status, fork, MCP) get GUI variants.
- **Performance:** `bench --client gui|tui --remote`; stress and perf probes run headless.

## 11. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Dev Tunnels drops or authentication problems | Stateless clients plus resync; heartbeat and backoff; GitHub authentication fallback; the tunnel host is a self-restarting manifest automation the Doctor watches; RDP stays the fallback |
| Two clients editing the document at once | The backend is the single writer; patches carry versions; the last edit wins per field, with a notice |
| Remote image paste depends on the Cloud PC clipboard | The backend sets the clipboard in the user session, then sends the key; the probe covers it |
| libghostty-vt C API is alpha | Pinned commit, thin wrapper, alacritty fallback, golden corpus on every update |
| Zig and the static build on Windows CI | Pin Zig; cache the lib by checksum; a separate CI job |
| GPUI churn and Windows maturity | Spike; pin a version; drawing stays behind `ViewModel` |
| No GPU on the Cloud PC | The GPU client's primary target is the home PC; locally on the Cloud PC the TUI stays the default unless the GUI meets the budgets |
| IME, AltGr, dead keys, Shift+Enter | Ghostty's encoder plus key table tests; dogfooding |
| Parity drift while features keep coming | After Phase 0, all logic lives in `canvas-core` or the backend; new features update both front ends or are marked TUI-only or GUI-only in §8 |

## 12. First two weeks

1. Workspace and crate skeleton; `canvas-core` with `App` state moved; TUI selftest green.
2. `Backend` trait with the in-process implementation; route the §6 reads through it.
3. `Emulator` trait with the alacritty adapter; golden-corpus recorder (`copilot-canvas record --session <id>`) and comparer.
4. TCP transport behind `--remote loopback`; heartbeat and resync; the selftest passes over loopback.
5. Document ownership moved to the backend, with patches and broadcast; two clients at once.
6. Pairing, the `CopilotCanvas-Tunnel` manifest, `devtunnel connect` management; first real home connection.
