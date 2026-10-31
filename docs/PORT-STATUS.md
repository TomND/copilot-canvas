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

### Phase 0 Groundwork — macOS: in progress · Linux: not started

- [ ] Windows dependencies target-gated (`windows`, `windows-sys`,
      `webview2-com` under `[target.'cfg(windows)'.dependencies]`)
- [ ] `canvas-sys` crate: `process`, `fs`, `spawn`, `host`, `ipc`, `net`,
      `secret` (file backend first), `open`, `clipboard`; Windows re-exports
      `canvas_common::{win, pipe, dpapi, auth}` unchanged
- [ ] `PathStyle` and `BackendInfo::platform` (D10)
- [ ] Ghostty for Unix targets (xtask `--target`, build script, per-target ABI
      manifests)
- [ ] CI jobs for the portable and `canvas-sys` tests on macOS (and Linux on
      `linux`)
- Exit: `cargo check --workspace` (with and without Ghostty) and `cargo test`
  of the touched crates green on the OS; Windows CI and `cargo xwin clippy`
  green.

### Phase 1 Service and TUI (M0, M1) — not started

- M0: REMOTE.md checklist 1–12 from a Mac TUI against the Windows service.
- M1: `cargo test --workspace`, `corpus check`, `selftest`, `selftest --remote
  loopback|faults|paired`, backend contract, `selftest agent --paste`,
  `bench --terminals 100` within PLAN §9.

### Phase 2 Lifecycle and automations — not started

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

- 2026-10-30: the orchestration Task tool was not available in the session
  that started the port, so the program manager does the work directly,
  macOS first, in the order of plan §9; workers can be added later.

## Owner-only items (do not block "done")

- Developer ID Application certificate and App Store Connect API key as
  repository secrets (signing, notarization; plan §8.1, §11 item 17).
- Manual checks: IME (Japanese, Chinese, Korean), dead keys, AltGr,
  VoiceOver (macOS), Orca (Linux); real Linux desktops (GNOME/KDE × X11/
  Wayland) and GPUs (NVIDIA, Intel/AMD).
- M0 checklist against the owner's Windows Cloud PC over the Dev Tunnel.

## Log

- 2026-10-30: status file created; Phase 0 started on `macos`.
