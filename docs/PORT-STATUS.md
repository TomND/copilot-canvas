# macOS and Linux port: status

> **macOS: done** (2026-11-01, branch `macos`; CI runs 41282516535 of
> 31cb33f and 41287607972 of 9266cdd with every job green: macOS with the
> parity suite blocking, hardening, goldens and packages, Linux, Windows and
> iOS, whose job then passed three times in a row; main's WI-023 and row 47
> merged): every phase's exit
> criteria that a machine can check pass on CI, blocking where the plan asks
> (the parity suite's 65 rows, the nine hardening checks and macOS's nine
> goldens block from 055cef3 on); what is left needs the owner (the checklist
> under "Owner-only items: macOS").
>
> **Linux: done** (2026-11-01, branch `linux`, runs of fadd7c0 and
> dadd6b9, the latter with main's WI-023 and row 47 merged): every
> phase's exit criteria that a machine can check pass on CI, blocking where
> the plan asks; what is left needs the owner (the checklist under
> "Owner-only items: Linux").

The working log of the port described in [MAC-LINUX-PLAN.md](MAC-LINUX-PLAN.md)
(approved 2026-10-30, every recommended option of its §11). This file is how
work resumes after any interruption: read it first, then `git log`.

- Branches: `macos` (shared Unix work lands here first) and `linux` (merges
  `macos` regularly, Linux-only code). Neither is merged into `main`.
- Worktrees: `../copilot-canvas-macos` (branch `macos`),
  `../copilot-canvas-linux` (branch `linux`). The main checkout is not touched.
- Every commit keeps Windows green: `cargo xwin clippy --target
  x86_64-pc-windows-msvc --workspace --exclude ghostty-vt-sys
  --no-default-features --all-targets -- -D warnings` locally, the Windows CI
  jobs on push.

## Phases (plan §9)

### Phase 0 Groundwork — macOS: done (CI jobs `ghostty-macos`, `macos`) · Linux: done (CI jobs `ghostty-linux`, `linux`)

- [x] Windows dependencies target-gated (`windows`, `windows-sys`,
      `webview2-com` under `[target.'cfg(windows)'.dependencies]`)
- [x] `canvas-sys` crate: `process`, `fs`, `spawn`, `host`, `ipc`, `net`,
      `secret` (file backend first), `open`, `clipboard`, `console`, `token`;
      Windows re-exports `canvas_common::{win, pipe, dpapi, auth}` unchanged
- [x] `PathStyle` and `BackendInfo::platform` (D10); service paths shown
      in the service's separators
- [x] the whole workspace (without Ghostty) builds, clippy-clean, on
      macOS; type-checks for x86_64 Linux; `cargo test --workspace
      --no-default-features` green on macOS
- [x] Ghostty for Unix targets: `cargo xtask build-ghostty --target
      aarch64-apple-darwin|x86_64-apple-darwin|x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu`
      → `target/ghostty/<commit>/<triple>/` (Windows layout unchanged);
      `libghostty-vt.a` links with no extra system libraries on macOS; the
      ABI manifest test checks the target it runs on (LP64 bindings pass)
- [x] CI: `ghostty-macos` (Zig 0.17.0 checked by SHA-256) and `macos`
      (clippy, workspace tests with Ghostty, alacritty-only build, corpus,
      TUI self-test, `--remote loopback|faults|paired`) jobs
- Exit: `cargo check --workspace` (with and without Ghostty) and `cargo test`
  of the touched crates green on the OS; Windows CI and `cargo xwin clippy`
  green.

### Phase 1 Service and TUI (M0, M1) — macOS: M1 done (CI `macos`; M0 is the owner's) · Linux: M1 done (CI, x86_64 and aarch64; M0 is the owner's)

- M0: REMOTE.md checklist 1–12 from a Mac TUI against the Windows service.
- M1: `cargo test --workspace`, `corpus check`, `selftest`, `selftest --remote
  loopback|faults|paired`, backend contract, `selftest agent --paste`,
  `bench --terminals 100` within PLAN §9.
- [x] service on a Unix socket, PTY kill path, remembered descendants and the
      start-up orphan sweep, priority classes (PRIO_DARWIN_BG), RLIMIT_NOFILE,
      login-environment resolution, Keychain / key-file secrets
- [x] locally on macOS: `cargo test --workspace` (with Ghostty), `corpus
      check` (26 entries, both engines), `selftest` (39 steps), `selftest
      --remote loopback|faults|paired`, backend contract
- [x] the same green on the CI `macos` job (every run since 41240250214:
      clippy, workspace tests, corpus with both engines, `selftest`,
      `--remote loopback|faults|paired`)
- [x] `selftest agent --paste` (OpenCode 1.18.35, Claude Code), `doctor`
- [x] `selftest agent --agent opencode --prompt`: status, wheel, push,
      subagent, resume, passive steps; the question and permission steps are
      skipped on OpenCode < 1.19 (known gap below)
- [x] `bench --terminals 100` on a release build (M1 Pro, 2026-11-01):
      keystroke → screen p50 4.9 / p99 6.5 ms, the service 3 % of one core,
      522 KB per terminal (PLAN §9's Windows row: 14.5 %, ~5 MB); CI runs it
      too (advisory, artifact `bench-terminals-macos`)
- Linux (CI `linux`, every runner): `cargo test --workspace` (backend
  contract included), `corpus check` with both engines, `selftest`,
  `selftest --remote loopback|faults|paired`; on ubuntu-24.04 OpenCode
  1.19.4 and Claude Code 2.1.309 from npm: `doctor` with no failing check
  and `selftest agent --paste` PASS for both (nothing submitted). `bench
  --terminals 100` (release, 4 vCPUs): keystroke → screen p50 3.9 / p99
  4.3 ms (x86_64), 3.8 / 4.0 ms (aarch64); the service 10–12 % of one
  core, about 650 KB per terminal (PLAN §9's Windows row: 14.5 %, ~5 MB).

### Phase 2 Lifecycle and automations — Linux: done (CI) · macOS: done (CI: `selftest automations` on launchd PASS, `service install` checked; a real tunnel is the owner's)

- **The `Scheduler` trait has landed** (0675e2e, `crates/canvas-agents/src/scheduler/`,
  owner: the Linux lead). The macOS lead adds `scheduler/launchd.rs`
  implementing `Scheduler` and one arm in `scheduler::choose()` (unix); the
  launcher-based helpers are in `scheduler/common.rs` (`action`,
  `task_json`, `slots`, `next_run`, `definition_drift`, `task_json_drift`),
  the built-in launcher in `launcher.rs`; `selftest automations` is the
  backend-neutral end-to-end check; `service_install.rs` has the macOS arm to
  fill.
- [x] `Scheduler` trait, Task Scheduler behind it unchanged (0675e2e)
- [x] built-in launcher `automations launch` (0675e2e); `automations remove`
- [x] Linux: systemd user units, cron fallback (129e7e6); `service install`
      as a systemd user unit (4465089); Secret Service secrets (3f5b1e6)
- [x] Linux: `selftest automations` green on CI on the runners' user
      systemd and cron (run 41230428742: PASS in 61 s / 61 s; a scheduled
      run fires within the minute on both)
- [x] Linux: `service install` checked on CI (the unit starts a service
      that answers, stop, uninstall) and clients start the service with
      `systemd-run --user` when a user manager runs (CI: `service start` →
      `copilot-canvas-default.service` active; the TUI self-test passes that
      way), run of 2e92d2e. A real Dev Tunnel hosted by the manifest is an
      owner check (no tunnel account on CI).
- [x] macOS: launchd backend and `service install` as a LaunchAgent
      (c9f410a): `selftest automations` PASS on launchd in 27 s locally
      (plists in a temporary folder through `COPILOT_CANVAS_LAUNCHD_DIR`,
      the probe's job unloaded afterwards); `service install` → the service
      answers within 3 s, `uninstall` leaves it running, nothing stays loaded;
      `doctor` shows `launchd (N task(s) installed)`

- `service install`; `Scheduler` trait; Launchd / SystemdUser / Cron; built-in
  launcher; import, apply, backups, drift, run, enable; tunnel manifest.
- Exit: TUI self-test automation steps and factory probes on the backend; a
  real tunnel hosted by the manifest; `doctor` clean.

### Phase 3 GUI core (M2) — macOS: done (CI) · Linux: done (CI; numbers on a real GPU are the owner's)

- Linux: `selftest --client gui` passes under Xvfb (X11) and headless weston
  (Wayland) and `--remote loopback` under X11, on Mesa's lavapipe, in the
  blocking `linux-gui` CI job; fonts from fontconfig, app id, icon; Wayland
  drag and drop on winit's own connection (f04e04f; winit 0.30 has none);
  typed keys for the benches through the X server (466a0ef). `bench
  --client gui` runs on CI as advisory (llvmpipe on a shared runner): first
  paint 420 ms cold / 321 ms warm, idle 1.5–2.6 % of a core with 10 live
  sessions, 74 MB with 4 tiles and 1,300 boxes, full frame of 1,300 boxes
  p50 21.7 ms; keystroke → present (keys typed through the X server since
  466a0ef) p50 28.5 / p99 31.9 ms and input → present p50 14.1 ms on
  llvmpipe: software rendering, so PLAN §9's 8 / 20 ms needs a real GPU
  (owner). X11 work areas come from the window manager (061e54d: Mutter's
  per-monitor `_GTK_WORKAREAS_D<n>`, else `_NET_WORKAREA` cut to the
  monitor); CI's hardening runs a tint2 panel so placement must keep out
  of it.

- [x] Metal (`os::GPU_BACKENDS`), fonts per OS (macOS: Cascadia Mono when
      installed, SF Mono, Menlo; system UI font; Apple fallbacks), test hooks
      over a Unix socket (short /tmp path when `$TMPDIR` is too long)
- [x] `selftest --client gui` (39 steps) and `--remote loopback` pass locally
      on macOS (Apple M1 Pro, Metal); CI steps added
- [x] D8 keys: `KeyPolicy::CommandApp` (Command = app shortcuts, Control =
      the program's, Command-click = Ctrl-click, Control-click = right click,
      left Option = Alt); posted keys (`NSApp postEvent`), work areas
      (`NSScreen.visibleFrame`), POSIX quoting of dropped paths
- [x] `bench --client gui` measured on CI (a locked Mac presents nothing):
      the macOS runner's paravirtual GPU presents, the release build's
      numbers are the `gui-bench-macos` artifact (advisory; a VM's GPU, not
      PLAN §9's reference machine). On real hardware: owner checklist
- [x] DPI changes, several windows, the crash log and device loss on macOS
      (the parity and hardening suites, Phase 5); IME: owner checklist
- [x] D15 trackpad (b84377f): `PointerPolicy::Precise` on macOS (pixel-exact
      pan with the system's momentum, one target per gesture, fractional
      terminal scrollback, pinch around the pointer and for text size, smart
      zoom, force click, Command+scroll zooms); Windows and Linux keep
      notches. Parity T1/T2 pass locally (85e186e)

- Exit: `selftest --client gui` (39 steps) local and `--remote loopback`;
  `bench --client gui` within PLAN §9.

### Phase 4 GUI platform integration — Linux: done (CI; IME, AltGr and Orca by ear are the owner's) · macOS: done (CI; owner's manual checks open)

- macOS: the HTML view (a17b069) is a WKWebView child view over the
  viewer's content area, locked down as the iPhone app's (page JavaScript
  off, the content rule list compiled into `<home>/webkit-rules`, only the
  page's own navigation, links asked about, no windows, link previews or
  inspector, a non-persistent store, camera and microphone denied); refused
  loads are counted by a script in an isolated world; Esc in it closes the
  report. Row 41b passes locally on a locked screen (WebKit renders
  snapshots without presents). The window capture is the GUI's frame with
  the view's snapshot at the view's own frame: macOS has no capture of a
  window's composition without the Screen Recording permission (a prompt).
- macOS, the rest (5fa77df, e495c12, 5cd296b, 2662318):
  - **Dock tile** (row G2's equivalent): the badge counts the sessions that
    need you, an amber dot then and a blue one while sessions work;
    attention bounces the icon (`requestUserAttention`); `in_front` from
    winit's focus events.
  - **Notifications**: UserNotifications from the app bundle (the first one
    asks); outside the bundle (the bare binaries, tests, CI) a toast records
    `not shown: …` with the reason, which rows 17 and G2 count as recorded
    only (plan §5.6). The bundled path is the owner's check (a permission
    prompt).
  - **Menu bar** in place of winit's, whose Quit ended the app without
    asking: Quit (⌘Q, the Dock's Quit, the 'quit' Apple Event) asks to
    detach or stop all, the other windows follow the window that asked;
    logout, restart and shutdown quit without asking (the service keeps the
    sessions). Edit and Window items act as the canvas keys (the report
    page's Copy while it has the keyboard); Settings…, Help. Row 29 posts a
    real ⌘Q through AppKit where the window is key (CI: passes).
  - **Reduced motion** from `NSWorkspace.accessibilityDisplayShouldReduceMotion`.
  - **Accessibility**: VoiceOver reads AccessKit's NSAccessibility tree; the
    hardening check walks it in-process (an outside AX client needs the
    Accessibility permission, a prompt) and finds the toolbar buttons, the
    sidebar's rows, the box with its status and the terminal as an
    `AXTextArea` whose value is its text, and presses + New through it.
  - **⌘0** resets a terminal's text size; hints say ⌘ where the app's
    shortcuts are Command (status line, the sidebar's RECENT keys).

- Linux: the HTML view (WebKitGTK offscreen helper) passes row 41b on X11
  and Wayland, Ctrl+C copies its selection; notifications
  (`org.freedesktop.Notifications`), the launcher entry, `in_front`,
  flashing (xdg-activation / urgency). D15: Wayland touchpads pan precisely
  (5cacb17, `os::scroll_policy`), T1/T2 pass under weston; X11 stays on
  notches. Accessibility: AT-SPI (AccessKit) with terminal text as text
  runs, read by an outside AT-SPI client in the hardening check (718bebd).
  Owner checks: IME (IBus/Fcitx), dead keys, AltGr, Orca by ear, GNOME/KDE
  docks.

- Exit: parity rows 17, 41, 41b, G2 pass; manual IME / dead-key / AltGr /
  VoiceOver / Orca checks recorded (owner).

### Phase 5 Parity, goldens, hardening — macOS: done (CI, blocking) · Linux: done (CI, blocking)

- macOS on CI (runs 41260077261, 41271941822 and 41282516535, the last
  blocking; visible windows on the runner's paravirtual GPU): **`PASS: 65 rows`** (62 + T1, T2 and main's
  47: 32 grid repaints in 2 s while working, none when idle; 7b and V3
  count their frames there). The suite blocks from now on. T2 failed now
  and then: its diagnostics showed the gesture over the agent's tile latched
  to Shell A. A began within 150 ms of a lift-off is that gesture's
  momentum (winit reports momentum and a new touch alike), and CI's release
  build opened the agent's tile that fast; the row now starts its new
  gestures a moment after the last lift-off (5a37471). The product behaves
  as designed. Locally on a locked Mac
  62 of 64 (7b, V3 and 47 need presents). Run alone, row 1 fails after its
  box drag (it needs the state the rows before it leave).
- macOS hardening on CI: **9 of 9** (blocking): toolbar counts, overlapping
  boxes, device loss, DPI, placement, the NSAccessibility tree, crash log,
  no software adapter's clear error, damage frames. Placement found a real
  bug: winit 0.30 put a restored window's content where its frame belongs,
  so the window moved up by its title bar on every start (2254a37).
- macOS goldens: the nine screenshots recorded on CI (scale 1), reviewed and
  committed as `tests/golden/gui/macos-1x` (aed6693); they match with 0
  pixels differing and block.
- **Shared parity fixes** (the macOS lead's; `linux` merges them):
  - 85e186e: G1 runs the demo through `test_shell::run_self`; G3 expects
    the local shell's quoting; the stand-in agent reports every mouse event
    of a read.
  - 883102c: row 14 discards its line without a bare Esc (zsh read it as
    Meta+p), `test_shell::discard` spaces the two interrupts (zsh held the
    second and dropped the next command's first key); row 18 clicks the
    path on the output's row (the typed command wraps onto a row with the
    path); row 44 and the self-test's upgrade steps run through
    `test_shell`; a failed 41b closes its report, so 22, 23 and 26 no longer
    fail behind it.
  - f876447 (product fix): the remote folder browser joins entries with the
    service's path style; on a macOS or Linux service V5 asked for
    `…/ai-docs\notes.md`.

- Linux (CI `linux-gui`, run of 0aa54e0, 2026-11-01): **`PASS: 62 rows in
  239.5s`** under X11 and 41b, T1, T2 under Wayland; **9 of 9 goldens**
  match Linux's own set (`tests/golden/gui/linux-1x`, bbe91dc); **9 of 9
  hardening checks** (placement under openbox, the accessibility tree over
  AT-SPI). All of them block from 45d164a. G3 and 33 had failed after the
  merge of 883102c: row 14 now reached the paste probe, whose raw terminal
  modes were never restored; bash keeps a finished command's modes, so
  Shell A echoed nothing and a pasted path never reached it (8cba4df,
  shared: `raw_vt_modes` restores them at exit, with a PTY test).
- Exit: `selftest --client gui --parity` PASS 60 rows; `--hardening` PASS;
  goldens recorded per OS.

### Phase 6 Packaging — macOS: done on CI (Developer ID signing waits for the owner) · Linux: done on CI (a KDE desktop install is the owner's)

- macOS (`packaging/macos/`, CI `macos-package`, da0c0f2):
  `packaging/macos/build.sh` builds both slices with their own Ghostty
  archive (Zig cross-compiles x86_64), joins them with `lipo` into the
  universal `Copilot Canvas.app` (the GUI is the bundle executable,
  `copilot-canvas` beside it; `Info.plist` with the plan's keys and folder
  usage texts; `AppIcon.icns` made from the icon's PNG with `sips` and
  `iconutil`; hardened runtime, ad hoc signed, no entitlements), the DMG
  (`CopilotCanvas-<ver>-macos-universal.dmg`, 41 MB, with an Applications
  link), its SHA-256 and the Homebrew cask filled in with it
  (`packaging/homebrew-tap/Casks/copilot-canvas.rb`: `app` plus `binary`;
  nothing stopped on uninstall, since `brew upgrade` uninstalls too).
  `sign.sh` signs with the Developer ID and notarizes the app and the DMG
  once the secrets exist (exits 0 with a note until then). `smoke.sh`
  installs from the DMG and runs it: the signature, both slices (the Intel
  one under Rosetta), the doctor, the TUI and GUI self-tests from the
  bundle. Locally all of that passed; CI also installs the cask from a local
  tap. The plan named `cargo xtask bundle-macos`; the shell script follows
  Linux's `packaging/linux/build.sh` (the work is lipo, iconutil, hdiutil,
  codesign and notarytool). The CLI follows a link to its real file
  (b6cc82e: Homebrew's link into PATH).
- Linux AppImage / `.deb` / `.rpm` / tarball. Signing and notarization of
  the macOS app: owner-blocked (Developer ID).
- Linux (`packaging/linux/`, CI `linux-package` on the glibc 2.35 floor,
  x86_64 and aarch64): artifacts `linux-packages-<arch>` hold
  `copilot-canvas_<ver>_<deb arch>.deb`, `copilot-canvas-<ver>.<arch>.rpm`,
  `Copilot_Canvas-<ver>-<arch>.AppImage` and
  `copilot-canvas-<ver>-linux-<arch>.tar.gz` (18–23 MB each; binaries need
  only libc, libm, libgcc_s, highest symbol GLIBC_2.35; GTK, WebKit,
  Vulkan, Wayland and X11 libraries are loaded at run time and recommended
  by the .deb and .rpm). `linux-package-test` installs them on clean
  runners: the .deb on 22.04, 24.04 and 24.04-arm (CLI self-test; the GUI
  self-test from the installed .deb on 24.04), the tarball into a prefix
  and back out, the AppImage extracted, the .rpm in a Fedora 41 container.
  appimagetool 1.9.1 and runtime 20251108 are pinned by SHA-256.

### Phase 7 Dogfood and rollout — macOS and Linux: docs done; dogfood and release are the owner's

- macOS: README's macOS section (the DMG and the cask, ⌘ as the app's key,
  the trackpad, Dock and notifications, launchd, building from source and
  the bundle; f8dc571), PARITY.md's macOS and Linux section, the plan's
  implementation status, this file. Signing, a release and the dogfood
  week are the owner's.

- Linux: README's Linux section (packages, run-time libraries, building
  from source and the packages; 2bdaed9), PARITY.md's macOS and Linux
  section, this file. Releases (publishing the CI packages) and the dogfood
  week are the owner's.

## Parity rows (PARITY.md; plan §6)

State per OS: `—` not started · `wip` in progress · `PASS` · `equiv` (the
plan §6.1 equivalent, documented). macOS: CI `macos` (visible windows on
the runner's paravirtual GPU), 65 of 65 on 2026-11-01 (run 41271941822). Linux: CI `linux-gui` (X11, Xvfb, Mesa
lavapipe), 63 of 63 rows on 2026-11-01 (run of dadd6b9, with main's row
47); 41b, T1 and T2 also under Wayland (headless weston), where the
touchpad rows apply.

| # | Feature | macOS | Linux |
|---|---|---|---|
| 1 | Canvas: drag, pan, zoom, Show all nodes, menus | PASS | PASS |
| 2 | Status (seven states), OSC 9;4, background subagents | PASS | PASS |
| 3 | Attention borders and tints; halo | PASS | PASS |
| 4 | Colour-blind palette; icons | PASS | PASS |
| 5 | Subagents from history | PASS | PASS |
| 6 | Subsessions; spawned-subsession detection | PASS | PASS |
| 7 | Loops | PASS | PASS |
| 7b | Smooth loop dot; reduced motion | PASS | PASS |
| 8 | MCP picker with presets | PASS | PASS |
| 9 | Groups, chips, launch defaults | PASS | PASS |
| 10 | Sidebar tree, filter, fly-to | PASS | PASS |
| 11 | Terminals: PTY, tiles, survive client exit | PASS | PASS |
| 11b | Detach and reattach | PASS | PASS |
| 12 | Wheel rules | PASS | PASS |
| 13 | User-message tint | PASS | PASS |
| 14 | Copy and paste | PASS | PASS |
| 15 | Image paste; thumbnail | PASS | PASS |
| 16 | Click forwarding in mouse mode | PASS | PASS |
| 17 | Title, taskbar state, bell, OSC 777 | PASS | PASS |
| 18 | Links: URLs, OSC 8, paths | PASS | PASS |
| 19 | New session dialog | PASS | PASS |
| 20 | Import; VS Code sync | PASS | PASS |
| 21 | Activity bar views | PASS | PASS |
| 22 | Copilot defaults, group overrides | PASS | PASS |
| 23 | Plugins, MCP, skills | PASS | PASS |
| 24 | Migrate from VS Code | PASS | PASS |
| 25 | Passive sessions | PASS | PASS |
| 26 | Automations | PASS | PASS |
| 27 | Copilot automations, session schedules | PASS | PASS |
| 28 | Idle auto-stop, priority manager, upgrade, crash log | PASS | PASS |
| 29 | Keyboard map, F6 / Ctrl+], quit dialog, help | PASS | PASS |
| 30 | `--demo` | PASS | PASS |
| 31 | `--perf` overlay | PASS | PASS |
| 32 | CLI | PASS | PASS |
| 33 | Remote mode | PASS | PASS |
| 34 | View versus start | PASS | PASS |
| 35 | Transcript inspector; child resume | PASS | PASS |
| 36 | Removing a multi-selection | PASS | PASS |
| 37 | Group background and border selection | PASS | PASS |
| 38 | Recent sessions | PASS | PASS |
| 39 | Default model per agent | PASS | PASS |
| 39b | Remembered model after restart | PASS | PASS |
| 40 | Canvas-wide agents | PASS | PASS |
| 40b | Canvas-wide agent runs across groups | PASS | PASS |
| 41 | Reports inbox, unread count, toast | PASS | PASS |
| 41b | HTML reports, locked down | PASS | PASS |
| 41c | Remote reports | PASS | PASS |
| 42 | Pull request coordination | PASS | PASS |
| 43 | Review follow-ups | PASS | PASS |
| 44 | Undo/redo of layout changes | PASS | PASS |
| 45 | Area selection of subsessions and subagents | PASS | PASS |
| 46 | Subagents folded into decks by default | PASS | PASS |
| 47 | Dot grid pulse while agents work (WI-023) | PASS | PASS |
| G1 | Inline images | PASS | PASS |
| G2 | Taskbar progress, flash, toasts | PASS | PASS |
| G3 | File drag and drop | PASS | PASS (Wayland drops: unit tests; owner on a desktop) |
| G4 | Several windows | PASS | PASS |
| V1 | Dust | PASS | PASS |
| V2 | Stacks and hidden subsessions | PASS | PASS |
| V3 | Effects | PASS | PASS |
| V4 | Group chips at any zoom; sidebar rows | PASS | PASS |
| V5 | Remote group files in the canvas | PASS | PASS |
| V6 | Remote group passive sessions | PASS | PASS |
| T1 | Trackpad on the canvas (D15) | PASS | PASS (Wayland; pan part) |
| T2 | Trackpad over terminals (D15) | PASS | PASS (Wayland) |

## Decisions taken during the port

(Where the plan is silent; each consistent with it.)

- 2026-10-31: the top-level session orchestrates two leads that work
  directly (no sub-agents): the macOS lead on `macos` (shared Unix code
  lands here first) and the Linux lead on `linux` (merges `macos`; shared
  fixes made on `linux` are marked "shared:" and merged back into `macos`).
- `canvas-sys` holds every non-GUI OS call (D1); on Windows it re-exports
  `canvas_common::{win, pipe, dpapi, auth, tcp}` and the moved Win32 code
  unchanged. Off Windows: `ipc` = Unix sockets with `flock` single
  instance and peer-uid checks; `secret` = `v1u:` envelope sealing with a
  per-home master key (key file backend first, Keychain next);
  `net` = libproc socket tables (macOS) and `/proc/net/tcp*` (Linux).
- GUI OS edges live in `canvas-gui/src/os/`: `windows` (unchanged Win32
  code), `macos` (macOS lead), `linux` (**the Linux lead's from now on**),
  `unix` (helpers both share: counters, clipboard images, the native handle,
  the text report view). Every module exports the same functions. The split
  has landed (2026-10-31); `macos` and `linux` start as identical stand-ins.
- Ownership (manager's decision, 2026-10-31): the Linux lead owns the
  `Scheduler` trait, the built-in automations launcher, the systemd-user and
  cron backends and Linux `service install` ("shared:" commits on `linux`);
  the macOS lead adds the launchd backend and macOS `service install` on top
  after merging them, and goes to Phase 3 (GUI on Metal) first.
- Automations (D7): one `Scheduler` trait (`list`, `task`, `definition`,
  `run`, `set_enabled`, `register`, `delete`, `render`, `import`, `drift`,
  `uses_task_json`, `task_json`, `importable`, `launched_by`); a definition
  is the backend's own text (`TaskInfo::xml` keeps its name for the
  protocol). Drift off Windows compares the rendered and the installed
  definition key by key plus the launcher's task file. Off Windows every run
  goes through the built-in launcher unless `automations.yaml` names its own
  `launcher`/`python`; the launcher enforces time limits, overlap (a lock
  and pid files per run), restarts and success codes; systemd gets
  `--detach` + `KillMode=process` for overlaps other than `ignore` (a timer
  never starts an active service). Cron: `start_when_available` has no
  equivalent, `logon` is `@reboot`, intervals not dividing a day restart
  daily. Default launcher state: `$XDG_STATE_HOME/background-automations`
  (Linux), `~/Library/Application Support/BackgroundAutomations` (macOS).
- `canvas-push` uses rustls (ring) on Linux, native-tls elsewhere (§5.9).
- **Linux HTML view (D9, §5.7): WebKitGTK 4.1 rendered offscreen**, not WPE,
  not Blitz, not an X11 child (spike 2026-10-31, `spikes/linux-html/`, CI
  workflow `linux-html-spike`, run 41222103421). Why:
  - **WPE is not packaged where we need it**: Ubuntu publishes `wpewebkit`
    only for 20.04 and 22.04 (2.36.0 from 2022, never updated), nothing for
    24.04 or later; Debian 12 has 2.38.6. Shipping it would mean bundling
    WebKit and owning its security updates. WebKitGTK 4.1 is maintained in
    every target distribution (22.04: 2.50.4, 24.04: 2.52.6, Debian, Fedora).
  - **Offscreen works on X11 and Wayland alike**: a `GtkOffscreenWindow`
    (no surface on screen, its own display connection) rendered the report
    under Xvfb and headless weston with `hardware-acceleration-policy=NEVER`
    (CPU, no GPU needed). The texture route keeps one code path and makes
    the parity capture the normal offscreen `png`.
  - **Fidelity**: WebKitGTK matches Chrome (the WebView2 engine) on the
    realistic report (`pages/brief.html`: gradient-clipped heading, flex
    cards, grid, table pills, `::marker`, `<kbd>`, dashed `<details>`, data:
    PNG, colour emoji, CJK) except for font faces. Blitz 0.2 lost the
    `background-clip:text` heading, colour emoji, `<kbd>` borders, marker
    colours and dashed borders, and laid the table out differently.
  - **Measured** (GitHub runners, 1000×800): first frame 155–890 ms cold
    (WebKit's processes start), 12–16 ms for the next report; snapshot of
    the view 6–10 ms (`webkit_web_view_get_snapshot`), offscreen pixbuf
    42–52 ms; memory (PSS) ≈ 80–114 MB for the GTK side, ≈ 82–96 MB
    WebProcess, ≈ 18–29 MB NetworkProcess. Blitz: 20–25 MB, 24–408 ms.
  - **Lock-down verified**: page JavaScript off at parse
    (`enable-javascript-markup=false`: script elements and handler
    attributes are removed, so the raw probe sees 0 scripts where WebView2
    keeps 1 inert one; the title never changed); `evaluate_javascript`
    needs `enable-javascript=true` ("Cannot execute JavaScript in this
    document" otherwise), so production keeps it off and the test hooks
    turn it on only around an `eval`; navigation refused in `decide-policy`
    (the script's `location.href` change was blocked, the page stayed; a
    clicked link arrives as a user-gesture `LINK_CLICKED` for the canvas to
    ask about); nothing reached the local listener. **WebKit preconnects to
    a clicked link's host** (`CONNECT example.com:443`) even though the
    navigation is refused and the content rule list is on: the view
    therefore gets a **deny proxy** (an in-process listener on 127.0.0.1
    answering 403 and counting, like WebView2's 403 answers) for http(s)
    and ws(s), and the iPhone's content rule list for file/blob/ftp.
  - **bubblewrap sandbox**: works on 22.04; on 24.04 (AppArmor restricts
    unprivileged user namespaces) bwrap fails and WebKit aborts the whole
    process (`g_error`). So the view runs in a **helper process**
    (`copilot-canvas-gui --html-view`, GTK and WebKit loaded with `dlopen`,
    nothing linked at build time) that talks to the GUI over a socket pair
    (commands one way, BGRA frames the other), and the sandbox is enabled
    only when a `bwrap` probe succeeds. Missing library or a failed helper:
    the formatted-text view with a note, as on Windows without WebView2.
  - Input: wheel and click forwarded as synthesized GDK events worked
    (scrollY 0 → 258; the click became a link request).
- Timing-sensitive `canvas-host` terminal tests run serially (a test-only
  mutex): on 3-core macOS runners they starved each other's 100 ms ticks.
- Canvas locks (`Store::open`) use an exclusive `flock` off Windows (retried
  for 300 ms: a lock just released can linger in a forked child until exec).
- Demo and test shells: `/usr/bin/env PS1='PS > ' /bin/zsh -f` on macOS
  (its /bin/bash 3.2 has no bracketed paste), `bash --noprofile --norc` on
  Linux, PowerShell on Windows; chosen by the service's platform
  (`doc::demo_shell`). The prompt is `PS > ` (not the plan's `PS> `) so the
  self-tests' PowerShell checks hold unchanged.
- Secrets backend on macOS: the Keychain only for Developer-ID-signed builds
  (unsigned development builds would prompt on every rebuild); a home keeps
  the backend it started with (`remote/master.key` or `remote/master.keychain`).
- The service merges the login shell's environment at start except in
  self-tests (`COPILOT_CANVAS_SELFTEST=1`) and with `COPILOT_CANVAS_LOGIN_ENV=0`.
- The graceful-stop timeline is measured on the clock (shared code): the
  documented 2 / 3.5 / 5 s and "3 s after the UI quit" now hold on a
  throttled machine too; Windows behaviour is otherwise unchanged.
- Copilot CLI processes on Unix are found by their command line (`node …/copilot`
  or any interpreter running a script named `copilot`); only those holding a
  session's in-use lock become subsessions.
- Copied text goes to the Mac (and Linux) clipboard with LF line breaks.
- macOS GUI decisions (Phase 4): winit's default menu replaced (its Quit
  ended the app without asking); toasts outside the app bundle record
  `not shown: …` instead of failing; the window capture of row 41b is the
  GUI's frame with the WKWebView's snapshot at the view's own frame (no
  composition capture without the Screen Recording permission); the
  accessibility check reads NSAccessibility in-process; the core knows the
  key policy for hints (`KeyPolicy::label`; the TUI and Windows keep Ctrl).
- launchd (D7, §5.5): `~/Library/LaunchAgents/com.copilot-canvas.task.<name>.plist`
  (the name `%XX`-escaped in the label), loaded into `gui/<uid>` (else
  `user/<uid>`). The task's name, manifest and schedule are XML comments in
  the plist. A disabled task keeps its plist with `Disabled` and is unloaded;
  enabled means loaded (`not loaded` otherwise, which `apply` repairs).
  Every minute is one empty `StartCalendarInterval` entry; an interval that
  does not divide a day is `StartInterval` from load (its `from` is lost);
  `logon` is `RunAtLoad`, which also runs the task once when installed or
  enabled; `start_when_available: false` cannot be honoured (launchd
  coalesces a missed run on wake). Replacing a loaded definition reloads the
  job and stops a run in progress.
- `service install` on macOS: the LaunchAgent runs `copilot-canvas service
  start` (`RunAtLoad`, `AbandonProcessGroup`), not the service itself as the
  plan's table had it, so launchd never owns the service: it keeps exiting
  when idle, clients restart it, and uninstalling (`bootout`) cannot stop it
  or its terminals (as on Linux, where uninstall only disables the unit).
- D15 on Linux: `os::scroll_policy()` is Precise on Wayland (touchpads give
  pixel deltas with phases) and Notches on X11 (XInput2 smooth scrolling
  gives phase-less fractional lines); `POINTER_POLICY` stays Notches (help
  text, pinch guards). Pinch, smart zoom and force click need winit's
  pointer gestures (not in 0.30): plan §6.1 and PARITY.md say so.
- Linux packages: binaries link only libc/libm/libgcc_s (GLIBC_2.35 at
  most); GTK/WebKit, Vulkan/GL, Wayland, X11, xkbcommon and fontconfig are
  `dlopen`ed and the .deb/.rpm *recommend* them (a minimal install still
  runs the CLI, service and TUI). The package licence is
  `LicenseRef-proprietary` until the owner names one (there is no LICENSE).
- Linux accessibility: AccessKit's AT-SPI bridge exposes text only through
  text runs, so on Linux a terminal tile's visible rows are also text runs
  (Windows keeps the UIA value only). The hardening check is an outside
  AT-SPI client: it turns `org.a11y.Status.IsEnabled` on (as Orca does) and
  back off afterwards when it was off (GNOME stores it as a setting).
- Linux CI runs the hardening checks under openbox: a bare X server has no
  window manager, so nothing maximizes a window there.
- Linux test hooks: posted keys go through the X server (focus, then
  `KeyPress`/`KeyRelease` sent to the window); Wayland lets no client type
  into another's surface, so the latency bench types under X11.

## Known gaps

- D15: Command+0 does not reset a terminal's text size yet (pinch and
  Command+plus/minus change it).

- OpenCode < 1.19 (1.18.35 on the development Mac): rejecting a question or
  a permission with Esc does not end the turn, so `selftest agent --agent
  opencode --prompt` skips its question and permission steps there (with a
  note). Verified behaviour is OpenCode 1.19.4's (docs/AGENTS.md); the
  owner's OpenCode is not updated by the port.

- Linux: pinch, smart zoom and force click (winit 0.30 has no pointer
  gestures on Wayland or X11); X11 touchpads scroll in notches. Wayland
  drag and drop is checked only by unit tests on CI (a headless drag has no
  pointer serial). With one desktop-wide `_NET_WORKAREA` (window managers
  other than Mutter), a panel on one monitor shrinks every monitor's work
  area (as in GTK).

## Owner-only items (do not block "done")

- Developer ID Application certificate and App Store Connect API key as
  repository secrets (signing, notarization; plan §8.1, §11 item 17).
- Manual checks: IME (Japanese, Chinese, Korean), dead keys, AltGr,
  VoiceOver (macOS), Orca (Linux); real Linux desktops (GNOME/KDE × X11/
  Wayland) and GPUs (NVIDIA, Intel/AMD).
- M0 checklist against the owner's Windows Cloud PC over the Dev Tunnel.
- **Owner-only items: macOS** (what remains after "macOS: done"):
  1. Signing: the Developer ID Application certificate and an App Store
     Connect API key as repository secrets (`MACOS_CERTIFICATE_P12` base64,
     `MACOS_CERTIFICATE_PASSWORD`, `MACOS_SIGN_IDENTITY`,
     `APPSTORE_API_KEY_P8` base64, `APPSTORE_API_KEY_ID`,
     `APPSTORE_API_ISSUER_ID`); CI's `macos-package` then signs, notarizes
     and staples the app and the DMG (`packaging/macos/sign.sh`).
  2. A tap repository (`homebrew-tap`) holding
     `packaging/homebrew-tap/Casks/copilot-canvas.rb` with a release's
     version and SHA-256 (build.sh writes it), and a GitHub release with the
     DMG.
  3. The installed app on a Mac: Gatekeeper opens the signed DMG's app
     without a warning; the first notification asks for permission, later
     ones show and are listed in Notification Center; the Dock badge and
     dot; ⌘Q and the Dock's Quit ask; `copilot-canvas service install`
     starts the service at the next login.
  4. Manual input: IME (Japanese, Chinese, Korean) and dead keys in a
     terminal and in the dialogs; VoiceOver reading the toolbar, the boxes
     and a terminal.
  5. Trackpad (D15) on hardware: two-finger pan and its momentum feel
     native with natural scrolling on and off; momentum stays on the box or
     terminal it started in; pinch, smart zoom (two-finger double tap) and
     force click on a Force Touch trackpad.
  6. `bench --client gui` on real hardware with the screen unlocked (PLAN
     §9: keystroke → present p50 ≤ 8 / p99 ≤ 20 ms, idle ≤ 1 %; CI's
     paravirtual GPU gives input → present p50 7.4 ms, keystroke → present
     p50 31 ms with the shell's echo, idle 2.2 %).
  7. M0 (REMOTE.md checklist 1–12) from a Mac against the Windows service,
     and a real tunnel hosted by the tunnel manifest on a Mac.
  8. Phase 7: a dogfood week on macOS and the 48 h remote soak with a Mac
     as client and as service.
- **Owner-only items: Linux** (what remains after "Linux: done"):
  1. A real tunnel hosted by the tunnel manifest on a Linux machine (Dev
     Tunnels account; plan Phase 2 exit).
  2. GNOME and KDE desktops, each under Wayland and X11: install the .deb
     (Ubuntu), the .rpm (Fedora) and the AppImage from the desktop; menu
     entry, icon, dock badge and progress (LauncherEntry), notifications,
     attention; first run with GNOME Keyring or KWallet.
  3. A file dragged from Files/Dolphin onto a terminal (Wayland and X11) and
     a folder onto the canvas.
  4. Manual input: IME (IBus or Fcitx: Japanese, Chinese, Korean), dead
     keys, AltGr; Orca reading the toolbar, boxes and a terminal.
  5. Touchpad under Wayland: two-finger pan and momentum feel native, the
     gesture stays on its target; X11 scrolls in notches (expected).
  6. `bench --client gui` on real GPUs (NVIDIA, Intel/AMD): PLAN §9's
     keystroke → present p50 ≤ 8 / p99 ≤ 20 ms and idle ≤ 1 % (CI's
     software renderer gives 28.5 / 31.9 ms and 1.5–2.6 %).
  7. Phase 7: a dogfood week on Linux and the 48 h remote soak with Linux
     as client and as service; publishing the CI packages as a release.
  8. The package licence: `LicenseRef-proprietary` stays until the owner
     names one (there is no LICENSE file).

## Log

- 2026-11-01 (macOS): **main merged at a640512** (Notebook, Tasks and Spaces;
  the ✦ Skills view; the repository's agent docs; row 47 → 51) into
  `macos`. Conflicts kept main's behaviour with the port's platform code:
  the help is per space with the trackpad column where scrolling is
  precise; main's UIA client (tabs, selection) moved into `src/uia.rs` and
  the accessibility check's tab steps go through the per-OS dispatch; the
  web view's new interface (`sync` with the backend, `blur`, `pump`,
  `print`, `Key`/`Navigate`/`Notice` events) is implemented on macOS and
  Linux, where HTML notes show a placeholder until their views are ported
  (macOS: next; Linux: the Linux lead's). Two of main's tests assumed a
  case-sensitive file system and distinct milliseconds per spooled report:
  fixed for macOS (shared).

- 2026-11-01: owner report: the macOS app opened from Finder did not see OpenCode (in `~/.opencode/bin`, put on PATH by `~/.zshrc` only). The app's in-process backend decides which agents are installed with the app's own PATH, which Finder and the Dock leave bare; only the service read the login environment. Fixed: the GUI reads the login environment too when it has no terminal (`TERM` unset), and every agent CLI is also looked for in the usual install folders after PATH (`~/.local/bin`, `~/.opencode/bin`, `~/.bun/bin`, `~/.npm-global/bin`, `~/.volta/bin`, Homebrew; none on Windows). `doctor` with a bare PATH now finds OpenCode.
- 2026-11-01 (macOS): merged origin/linux (row 47's margin and diagnostics,
  the accessibility check's client name) and with it main's WI-023 (row
  47); **macOS: done**. CI run 41271941822: parity 65 of 65, hardening 9 of
  9, goldens 9 of 9, packages built, installed and smoke-tested; the parity
  suite and the hardening checks block on macOS (055cef3). iOS CI: the UI tests
  wait for both mocks' ports and a booted simulator, run on one simulator,
  give each test's first connection 90 s and look for the inbox row again
  before tapping it (98817b7); a failure keeps the result bundle and the
  mocks' logs. Windows: the self-test's "open the other client" step read
  the dry-run file before it was written (an empty read; fixed); the reports
  test wrote a spool file in place while the service polled (a half-written
  read on a Windows runner; it spools as the CLI does now, 44e1d05);
  `opencode_history`'s git push test failed once on a Windows runner (40 of
  40 passes on macOS; left to main). iOS CI, second cause: the keyboard
  checks allowed 5 s while each look took up to a second on a busy runner;
  15 s now, and the simulator boots without a hardware keyboard (464a24c);
  third: instant `isHittable` checks right after the keyboard or the layout
  moved now wait for the element to become hittable; fourth, once the
  on-screen keyboard showed: iOS's one-time slide-to-type tip covered the
  keyboard and the key bar on a fresh simulator (the result bundle's
  screenshot), so the simulator is told it was shown and the tests close it
  if it appears anyway. The checks themselves are unchanged. `selftest
  automations` reads the newest *finished* run (the every-minute schedule
  can start another one right after "run now"; seen on Linux's systemd).

- 2026-10-30: status file created; Phase 0 started on `macos`.
- 2026-10-31: canvas-sys, the terminal host, core, backend, TUI, CLI and
  GUI build on macOS (443cc87…ed67588); Unix twins of the Windows-only
  tests; CI fixes for the macOS `ios` job (serial timing tests, EINVAL on
  reset sockets). Next: Ghostty for Unix targets, CI `macos` job, M0.
- 2026-10-31: merged origin/main (WI-021, WI-022); Ghostty built for
  aarch64-apple-darwin (85d383a); Keychain secrets (5530fe3); TUI self-test,
  remote loopback/faults/paired and corpus green locally on macOS
  (6fa2a0a, 7cdfd21); service start-up on Unix (e9a9a01); doctor (02f8bc6);
  `selftest agent --paste` passes for OpenCode and Claude Code; merged
  origin/linux for its shared fixes (0c73fea, 3f5b1e6, 998f959).
- 2026-10-31: GUI OS split (1d66e91); GUI self-test and `--remote loopback`
  on Metal (8c3c87e); D8 keys (06c3a8f); parity suite ported to the Unix
  shells (1c7975f, 44/62 locally). Windows parity rows 14/G1 and the `ios`
  job also fail on main (41228257005): main's, not the port's.
- 2026-10-31: merged origin/linux and origin/main (8cdd05a). D15 trackpad
  (b84377f) and its parity rows T1/T2 (85e186e). Found on the way: a
  trackpad pan within 800 ms of a DPI change was undone by the GUI's
  recentring (now the user's view is kept); the stand-in agent printed only
  the first mouse report of a read; G1 typed PowerShell syntax and G3
  expected Windows quoting on macOS. Merged origin/main and origin/linux
  again (a8744b2, 92f2dfe; main's heartbeat test fix answers the `macos`
  CI failure of run 41233445777). macOS sends a scale-factor event with
  the window's own scale as it first shows; the GUI refitted and recentred
  for 800 ms, so early clicks that opened a panel moved the view and rows
  1, 45, 46, V2 passed or failed by timing (e635e2a). Parity 54/64 locally.

### Linux log

(The Linux lead's entries, on `linux`. Commits whose message starts with
`shared:` touch shared code and are for `macos` to merge.)

- 2026-10-31: `linux` merged `origin/macos` (ed67588, Phase 0 groundwork).
  `cargo check` and `cargo clippy -D warnings` of the whole workspace
  (`--no-default-features`, Ghostty excluded) are clean for
  `x86_64-unknown-linux-gnu` from the Mac (C build scripts through
  `zig cc -target x86_64-linux-gnu.2.35`, Zig 0.17.0 in `~/.local/zig-0.17.0`).
  CI job `linux` added (ubuntu-24.04, ubuntu-22.04 = glibc 2.35 floor,
  ubuntu-24.04-arm): clippy, `cargo test --workspace`, release build;
  alacritty engine only until libghostty-vt builds for Linux targets.
- 2026-10-31: HTML view spike done (see Decisions): WebKitGTK 4.1 offscreen
  in a dlopen'ed helper process, deny proxy + content rule list, sandbox
  when bwrap works. WPE rejected (not packaged on Ubuntu 24.04; 2.36 on
  22.04), Blitz rejected (fidelity).
- 2026-10-31: Linux CI green on all three runners without Ghostty (run
  41223516059: clippy, workspace tests, release build). Shared fixes found
  by it: `newest_match` globbing, OpenCode folders kept in the local style,
  a heartbeat test race (0c73fea). The Unix demo shells / TUI self-test port
  was done twice in parallel (here and 6fa2a0a on `macos`); the `linux` copy
  was dropped before pushing and `macos`' kept.
- 2026-10-31: `ghostty-linux` CI job (x86_64 on ubuntu-24.04, aarch64 on
  ubuntu-24.04-arm; Zig 0.17.0 checksums from ziglang.org's index) and the
  Linux jobs now run with Ghostty, the alacritty-only build, the corpus with
  both engines, `doctor` and the TUI self-tests (`selftest`, `--remote
  loopback|faults|paired`) under Xvfb with xclip.
- 2026-10-31: Secret Service backend (3f5b1e6, shared): master key in the
  session keyring when one answers, key file otherwise; CI tests it against
  GNOME Keyring on a private `dbus-run-session` bus.

Linux next: the TUI self-tests green on CI; Phase 2 systemd/cron (needs the
`Scheduler` trait: owner of the trait to be agreed with `macos`); the HTML
view helper (WebKitGTK offscreen); the GUI on Wayland/X11.
- 2026-10-31: `--remote paired` green on all three Linux runners with the
  `spawn_tied` fix (998f959, run 41228017431); Windows `test`, `macos`,
  `ios` green on that run. A flaky listener test on arm64 fixed (1711e7c).
- 2026-10-31: Phase 2 on Linux: the `Scheduler` trait + built-in launcher
  (0675e2e, shared), systemd user units and cron (129e7e6), `service
  install|uninstall` + `selftest automations` + doctor's scheduler line
  (4465089, shared); CI runs the probe on the runner's user systemd
  (lingering) and on cron. A `linux-gui` CI job (Xvfb and headless weston,
  lavapipe/llvmpipe, dbus, fonts) reports the GUI self-test without
  blocking until Phase 3.
- Linux TODO (D5): clients should start the service with `systemd-run
  --user` when a user manager runs (own cgroup, out of the terminal's
  scope), passing the client's environment through a 0600
  `EnvironmentFile`; today it is the `setsid` spawn on every Unix.
- `canvas_model::bus::Local::now()` used UTC off Windows (passive-session
  schedules fired in UTC on macOS and Linux): local time through
  `localtime_r` now (fca7f01, shared).
- 2026-10-31: **the GUI self-test passes on Linux** on its first CI run
  (run 41230428742, job `linux-gui`): under Xvfb (X11) in 135 s, first
  paint 583 ms, and under headless weston (Wayland) in 133 s, first paint
  301 ms, on Mesa's lavapipe (Vulkan). Screenshots showed missing glyphs
  for ⧉ and ⏻ (DejaVu has neither): fallback fonts are now found by
  character with fontconfig and CI installs Noto's core fonts. The
  `linux-gui` job is blocking now (X11, X11 over loopback TCP, Wayland) and
  runs the parity suite as a report.
- 2026-10-31: GUI `os/linux` (after the split): fonts from fontconfig
  (named files, no full scan), app id `copilot-canvas` (Wayland app_id, X11
  WM_CLASS) and the X11 window icon, notifications through
  `org.freedesktop.Notifications`, the launcher entry (`LauncherEntry`:
  progress while sessions work, urgent when one needs you).
- 2026-10-31: first Linux parity run (X11, CI, before the HTML view): 49
  of 62 rows pass. Failing: 41b (the HTML view; landed since) and 22, 23,
  26 after it (its report stayed open); 12, 14, 18, G1, 44 (terminal rows
  still typing Windows text: shared with macOS, being ported there); 17
  (winit cannot read an X11 title back: the hooks now report the title set,
  shared); G3, V5 (to look at after the shared fixes).
- 2026-10-31: **HTML report view on Linux** (542a82a): WebKitGTK 4.1 in a
  helper process (the GUI binary with `COPILOT_CANVAS_HTML_VIEW=1`, GTK and
  WebKit `dlopen`ed), offscreen, frames drawn over the report pane through
  the shared hooks `ReportWeb::frame()`/`input()` (5504962, 2424845; no-ops
  on Windows and macOS), wheel (line or pixel deltas with the touchpad
  phase), pointer, clicks and keys forwarded (keys after a click into it,
  Esc closes); deny proxy + content rule list + decide-policy as decided;
  page zoom follows the window scale. `in_front` and flashing through the
  shared `os::track_window` hook (xdg-activation / X11 urgency).
- 2026-10-31: D5 on Linux: clients start the service with `systemd-run
  --user` when a user manager runs (shared, canvas-sys `start_user_unit`);
  `service install`'s CI check failed only because it did not wait for the
  service (which reads the login environment first, up to 5 s).
- Touchpads on Linux (for the macOS lead's `PointerPolicy`): winit 0.30.13
  gives Wayland touchpads `PixelDelta` with phases Started/Moved/Ended
  (`axis_stop`) and mouse wheels `LineDelta`; X11 (XInput2 smooth
  scrolling) gives fractional `LineDelta` with phase Moved only (no pixel
  units, no start or end); **pinch is not available on Linux in winit
  0.30** (`PinchGesture` is macOS/iOS only): a known gap until winit adds
  Wayland pointer gestures. Linux waits for the macOS lead's note before
  touching canvas-core's input model.
- 2026-10-31: D15 on Linux (5cacb17, shared: `os::scroll_policy`; ca9905d:
  docs, CI runs 41b, T1, T2 under Wayland, blocking). Wayland drag and
  drop (f04e04f, shared: `os::take_dropped`; `os/linux/dnd.rs` on winit's
  own Wayland connection). HTML view: Ctrl+C copies the selection.
- 2026-10-31: Linux packages (`packaging/linux/`, CI `linux-package` and
  `linux-package-test`; see Phase 6). The .deb's GUI did not start on a
  clean 24.04 runner until the packages recommended the run-time libraries
  (79669b5). CI also runs the hardening checks and the GUI bench under X11
  (reporting) and records Linux's goldens; the golden scripts' terminal
  text has a POSIX twin for the Unix demo shells (3c1b8ef, shared).
- 2026-10-31: merged origin/macos (2432279: launchd, the macOS HTML view,
  883102c's terminal-row fixes, f876447). Parity on X11: 60 of 62 (G3 and
  33 regressed after G1 on Linux only; macOS CI passes both). Goldens
  committed (bbe91dc). Hardening: AT-SPI client and terminal text runs
  (718bebd, shared), openbox for the placement check (1e8c102); typed keys
  through the X server for the bench (466a0ef).
- 2026-11-01: **Linux parity 62 of 62** (X11) after the paste probe's
  terminal modes were restored at exit (8cba4df, shared; found through a
  listing of the PTY's processes added to rows G3 and 33: bash alone, in
  the foreground, waiting in `poll`, with commands no longer echoed).
  Goldens compare 9 of 9, hardening 9 of 9 (AT-SPI roles from `GetRole`,
  ca88f10); parity and hardening block (45d164a). X11 work areas (061e54d),
  CI with a tint2 panel; the terminal bench on every Linux runner
  (advisory). README Linux section (2bdaed9), PARITY.md per OS, plan D15
  and PARITY.md T1/T2 for Linux (14737e5). Merged origin/macos (6f4f9b6:
  menu bar, Dock, notifications, accessibility and goldens on macOS).
- 2026-11-01: run of fadd7c0 green, `ios` on a re-run (its simulator UI
  tests failed twice before, a different test each time; the job also fails
  2 of the last 6 runs on `main`, and its mock never reaches the code
  changed here): parity 62/62
  and hardening 9/9 blocking, a tint2 panel gives the work area
  [0,0,1920,1050] and placement centres in it; doctor clean and the
  real-agent paste checks PASS with OpenCode and Claude Code installed;
  the terminal bench within PLAN §9. **Linux: done**; the owner's
  checklist is under Owner-only items.
- 2026-11-01: merged origin/macos (54d42fd) and origin/main (42c742f:
  WI-023, the dot grid's pulse, parity row 47). Row 47 failed once on
  Linux (14 full frames with the pulse against 9 without, 3 allowed) and
  passed on the next run (10 and 8): the spinner's full frames follow the
  core's tick, which a grid repaint delays on llvmpipe. The row now names
  skipped presents and window-system redraws when it fails (dadd6b9) and
  allows half the grid repaints (1812362, shared). Run of dadd6b9: every
  job green, `PASS: 63 rows in 248.3s` on X11.
