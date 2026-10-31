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

### Phase 2 Lifecycle and automations — Linux: Secret Service backend done (3f5b1e6); systemd/cron not started

- `service install`; `Scheduler` trait; Launchd / SystemdUser / Cron; built-in
  launcher; import, apply, backups, drift, run, enable; tunnel manifest.
- Exit: TUI self-test automation steps and factory probes on the backend; a
  real tunnel hosted by the manifest; `doctor` clean.

### Phase 3 GUI core (M2) — not started

- Exit: `selftest --client gui` (39 steps) local and `--remote loopback`;
  `bench --client gui` within PLAN §9.

### Phase 4 GUI platform integration — not started

- Exit: parity rows 17, 41, 41b, G2 pass; manual IME / dead-key / AltGr /
  VoiceOver / Orca checks recorded (owner).

### Phase 5 Parity, goldens, hardening — not started

- Exit: `selftest --client gui --parity` PASS 60 rows; `--hardening` PASS;
  goldens recorded per OS.

### Phase 6 Packaging — not started

- macOS `.app` / DMG (universal); Linux AppImage / `.deb` / `.rpm` / tarball.
  Signing and notarization: owner-blocked (Developer ID).

### Phase 7 Dogfood and rollout — not started

## Parity rows (PARITY.md; plan §6)

State per OS: `—` not started · `wip` in progress · `PASS` · `equiv` (the
plan §6.1 equivalent, documented).

| # | Feature | macOS | Linux |
|---|---|---|---|
| 1 | Canvas: drag, pan, zoom, Show all nodes, menus | — | — |
| 2 | Status (seven states), OSC 9;4, background subagents | — | — |
| 3 | Attention borders and tints; halo | — | — |
| 4 | Colour-blind palette; icons | — | — |
| 5 | Subagents from history | — | — |
| 6 | Subsessions; spawned-subsession detection | — | — |
| 7 | Loops | — | — |
| 7b | Smooth loop dot; reduced motion | — | — |
| 8 | MCP picker with presets | — | — |
| 9 | Groups, chips, launch defaults | — | — |
| 10 | Sidebar tree, filter, fly-to | — | — |
| 11 | Terminals: PTY, tiles, survive client exit | — | — |
| 11b | Detach and reattach | — | — |
| 12 | Wheel rules | — | — |
| 13 | User-message tint | — | — |
| 14 | Copy and paste | — | — |
| 15 | Image paste; thumbnail | — | — |
| 16 | Click forwarding in mouse mode | — | — |
| 17 | Title, taskbar state, bell, OSC 777 | — | — |
| 18 | Links: URLs, OSC 8, paths | — | — |
| 19 | New session dialog | — | — |
| 20 | Import; VS Code sync | — | — |
| 21 | Activity bar views | — | — |
| 22 | Copilot defaults, group overrides | — | — |
| 23 | Plugins, MCP, skills | — | — |
| 24 | Migrate from VS Code | — | — |
| 25 | Passive sessions | — | — |
| 26 | Automations | — | — |
| 27 | Copilot automations, session schedules | — | — |
| 28 | Idle auto-stop, priority manager, upgrade, crash log | — | — |
| 29 | Keyboard map, F6 / Ctrl+], quit dialog, help | — | — |
| 30 | `--demo` | — | — |
| 31 | `--perf` overlay | — | — |
| 32 | CLI | — | — |
| 33 | Remote mode | — | — |
| 34 | View versus start | — | — |
| 35 | Transcript inspector; child resume | — | — |
| 36 | Removing a multi-selection | — | — |
| 37 | Group background and border selection | — | — |
| 38 | Recent sessions | — | — |
| 39 | Default model per agent | — | — |
| 39b | Remembered model after restart | — | — |
| 40 | Canvas-wide agents | — | — |
| 40b | Canvas-wide agent runs across groups | — | — |
| 41 | Reports inbox, unread count, toast | — | — |
| 41b | HTML reports, locked down | — | — |
| 41c | Remote reports | — | — |
| 42 | Pull request coordination | — | — |
| 43 | Review follow-ups | — | — |
| 44 | Undo/redo of layout changes | — | — |
| 45 | Area selection of subsessions and subagents | — | — |
| G1 | Inline images | — | — |
| G2 | Taskbar progress, flash, toasts | — | — |
| G3 | File drag and drop | — | — |
| G4 | Several windows | — | — |
| V1 | Dust | — | — |
| V2 | Stacks and hidden subsessions | — | — |
| V3 | Effects | — | — |
| V4 | Group chips at any zoom; sidebar rows | — | — |
| V5 | Remote group files in the canvas | — | — |

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
- GUI OS edges live in `canvas-gui/src/os/{windows,unix}`; the Unix side is
  a stub (no toasts, no Dock state, text report view) until Phase 4.
- Task Scheduler stays Windows-only (`taskschd_none.rs` lists nothing and
  refuses changes elsewhere) until the `Scheduler` backends of Phase 2.
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
