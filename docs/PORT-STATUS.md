# macOS and Linux port: status

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

### Phase 0 Groundwork — macOS: done (CI confirming) · Linux: in progress (CI job `linux`)

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

### Phase 1 Service and TUI (M0, M1) — macOS: in progress · Linux: in progress (self-tests on CI)

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
- [ ] the same green on the CI `macos` job
- [x] `selftest agent --paste` (OpenCode 1.18.35, Claude Code), `doctor`
- [x] `selftest agent --agent opencode --prompt`: status, wheel, push,
      subagent, resume, passive steps; the question and permission steps are
      skipped on OpenCode < 1.19 (known gap below)
- [ ] `bench --terminals 100` on a release build (debug build, loaded
      machine: keystroke p50 5.5 ms, p99 7.2 ms; service 7 % of one core,
      0.5 MB per terminal)

### Phase 2 Lifecycle and automations — Linux: done (CI) · macOS: launchd pending

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
- [ ] macOS: launchd backend, `service install` LaunchAgent

- `service install`; `Scheduler` trait; Launchd / SystemdUser / Cron; built-in
  launcher; import, apply, backups, drift, run, enable; tunnel manifest.
- Exit: TUI self-test automation steps and factory probes on the backend; a
  real tunnel hosted by the manifest; `doctor` clean.

### Phase 3 GUI core (M2) — macOS: in progress · Linux: self-tests done (CI)

- Linux: `selftest --client gui` passes under Xvfb (X11) and headless weston
  (Wayland) and `--remote loopback` under X11, on Mesa's lavapipe, in the
  blocking `linux-gui` CI job; fonts from fontconfig, app id, icon. Still
  open: `bench --client gui` numbers on Linux, Wayland drag and drop (winit
  0.30 has none), XTest posted keys, X11 work areas.

- [x] Metal (`os::GPU_BACKENDS`), fonts per OS (macOS: Cascadia Mono when
      installed, SF Mono, Menlo; system UI font; Apple fallbacks), test hooks
      over a Unix socket (short /tmp path when `$TMPDIR` is too long)
- [x] `selftest --client gui` (39 steps) and `--remote loopback` pass locally
      on macOS (Apple M1 Pro, Metal); CI steps added
- [x] D8 keys: `KeyPolicy::CommandApp` (Command = app shortcuts, Control =
      the program's, Command-click = Ctrl-click, Control-click = right click,
      left Option = Alt); posted keys (`NSApp postEvent`), work areas
      (`NSScreen.visibleFrame`), POSIX quoting of dropped paths
- [ ] `bench --client gui` within PLAN §9: needs an unlocked, uncovered
      screen (macOS reports a covered window occluded and wgpu presents
      nothing); measured on CI (advisory step), the local Mac was locked
- [ ] IME, DPI changes, several windows, crash log and device loss checked
      on macOS (the parity and hardening suites, Phase 5)
- [x] D15 trackpad (b84377f): `PointerPolicy::Precise` on macOS (pixel-exact
      pan with the system's momentum, one target per gesture, fractional
      terminal scrollback, pinch around the pointer and for text size, smart
      zoom, force click, Command+scroll zooms); Windows and Linux keep
      notches. Parity T1/T2 pass locally (85e186e)

- Exit: `selftest --client gui` (39 steps) local and `--remote loopback`;
  `bench --client gui` within PLAN §9.

### Phase 4 GUI platform integration — Linux: rows 17, 41, 41b, G2 pass (CI)

- Linux: the HTML view (WebKitGTK offscreen helper) passes row 41b on X11
  and Wayland; notifications (`org.freedesktop.Notifications`), the
  launcher entry, `in_front`, flashing (xdg-activation / urgency). Owner
  checks: IME (IBus/Fcitx), dead keys, AltGr, Orca, GNOME/KDE docks.

- Exit: parity rows 17, 41, 41b, G2 pass; manual IME / dead-key / AltGr /
  VoiceOver / Orca checks recorded (owner).

### Phase 5 Parity, goldens, hardening — macOS: started

- 2026-10-31, locally on a locked Mac (no presents): 54 of 64 rows pass
  (62 + T1, T2). Failing: 7b and V3 (frame counts: need an uncovered
  screen), 41b (HTML view: Phase 4), 22, 23 (settings and MCP views from the
  stand-in CLI), 26 (automations: launchd backend, Phase 2), 14 (a long line
  wraps differently; also fails on main's Windows run 41228257005), 18
  (Ctrl+click on a path), 44 (paste probe), V5. Run alone, row 1 fails after
  its box drag (it needs the state the rows before it leave).

- Exit: `selftest --client gui --parity` PASS 60 rows; `--hardening` PASS;
  goldens recorded per OS.

### Phase 6 Packaging — not started

- macOS `.app` / DMG (universal); Linux AppImage / `.deb` / `.rpm` / tarball.
  Signing and notarization: owner-blocked (Developer ID).

### Phase 7 Dogfood and rollout — not started

## Parity rows (PARITY.md; plan §6)

State per OS: `—` not started · `wip` in progress · `PASS` · `equiv` (the
plan §6.1 equivalent, documented). macOS: the full suite run locally on a
locked Mac, 2026-10-31 (CI's run pending); 7b and V3 count frames, which a
locked screen never presents. Linux: CI `linux-gui` (X11, Xvfb, Mesa
lavapipe), 55 of 62 rows on 2026-10-31 (run of 2e92d2e); 41b also on
Wayland (headless weston).

| # | Feature | macOS | Linux |
|---|---|---|---|
| 1 | Canvas: drag, pan, zoom, Show all nodes, menus | PASS | PASS |
| 2 | Status (seven states), OSC 9;4, background subagents | PASS | PASS |
| 3 | Attention borders and tints; halo | PASS | PASS |
| 4 | Colour-blind palette; icons | PASS | PASS |
| 5 | Subagents from history | PASS | PASS |
| 6 | Subsessions; spawned-subsession detection | PASS | PASS |
| 7 | Loops | PASS | PASS |
| 7b | Smooth loop dot; reduced motion | wip | PASS |
| 8 | MCP picker with presets | PASS | PASS |
| 9 | Groups, chips, launch defaults | PASS | PASS |
| 10 | Sidebar tree, filter, fly-to | PASS | PASS |
| 11 | Terminals: PTY, tiles, survive client exit | PASS | PASS |
| 11b | Detach and reattach | PASS | PASS |
| 12 | Wheel rules | PASS | wip (shared terminal rows) |
| 13 | User-message tint | PASS | PASS |
| 14 | Copy and paste | wip | wip (shared terminal rows) |
| 15 | Image paste; thumbnail | PASS | PASS |
| 16 | Click forwarding in mouse mode | PASS | PASS |
| 17 | Title, taskbar state, bell, OSC 777 | PASS | PASS |
| 18 | Links: URLs, OSC 8, paths | wip | wip (shared terminal rows) |
| 19 | New session dialog | PASS | PASS |
| 20 | Import; VS Code sync | PASS | PASS |
| 21 | Activity bar views | PASS | PASS |
| 22 | Copilot defaults, group overrides | wip | PASS |
| 23 | Plugins, MCP, skills | wip | PASS |
| 24 | Migrate from VS Code | PASS | PASS |
| 25 | Passive sessions | PASS | PASS |
| 26 | Automations | wip | PASS |
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
| 41b | HTML reports, locked down | wip | PASS |
| 41c | Remote reports | PASS | PASS |
| 42 | Pull request coordination | PASS | PASS |
| 43 | Review follow-ups | PASS | PASS |
| 44 | Undo/redo of layout changes | wip | wip (shared terminal rows) |
| 45 | Area selection of subsessions and subagents | PASS | PASS |
| 46 | Subagents folded into decks by default | PASS | PASS |
| G1 | Inline images | PASS | wip (shared terminal rows) |
| G2 | Taskbar progress, flash, toasts | PASS | PASS |
| G3 | File drag and drop | PASS | wip |
| G4 | Several windows | PASS | PASS |
| V1 | Dust | PASS | PASS |
| V2 | Stacks and hidden subsessions | PASS | PASS |
| V3 | Effects | wip | PASS |
| V4 | Group chips at any zoom; sidebar rows | PASS | PASS |
| V5 | Remote group files in the canvas | wip | wip |
| V6 | Remote group passive sessions | PASS | PASS |
| T1 | Trackpad on the canvas (D15) | PASS | — |
| T2 | Trackpad over terminals (D15) | PASS | — |

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

## Known gaps

- D15: Command+0 does not reset a terminal's text size yet (pinch and
  Command+plus/minus change it).

- OpenCode < 1.19 (1.18.35 on the development Mac): rejecting a question or
  a permission with Esc does not end the turn, so `selftest agent --agent
  opencode --prompt` skips its question and permission steps there (with a
  note). Verified behaviour is OpenCode 1.19.4's (docs/AGENTS.md); the
  owner's OpenCode is not updated by the port.

## Owner-only items (do not block "done")

- Developer ID Application certificate and App Store Connect API key as
  repository secrets (signing, notarization; plan §8.1, §11 item 17).
- Manual checks: IME (Japanese, Chinese, Korean), dead keys, AltGr,
  VoiceOver (macOS), Orca (Linux); real Linux desktops (GNOME/KDE × X11/
  Wayland) and GPUs (NVIDIA, Intel/AMD).
- M0 checklist against the owner's Windows Cloud PC over the Dev Tunnel.
- Trackpad (D15) on hardware: two-finger pan and its momentum feel native
  with natural scrolling on and off; momentum stays on the box or terminal
  it started in; pinch, smart zoom (two-finger double tap) and force click
  (System Settings › Trackpad › Force Click) on a Force Touch trackpad.
- With the screen unlocked and the window uncovered: `bench --client gui`,
  parity rows 7b and V3 (if CI's macOS runners cannot present either).

## Log

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
