# Copilot Canvas on macOS and Linux: plan

Status: approved 2026-10-30 (owner: Thomas): every recommended option of §11
(decisions recorded there). Implementation on the branches `macos` and
`linux` (macOS first; shared Unix work lands on `macos` and is merged into
`linux`).
Baseline: `main` at b29b566. Goal: the service, the TUI, the GPU client and
every tool around them (CLI, automations, remote mode, reports, self-tests,
the parity suite) on macOS and Linux with **1:1 feature parity** with the
Windows version (every row of [PARITY.md](PARITY.md)), while the Windows code
stays as it is.

## 1. Summary

**Effort headline** (§9 has the phases; assumptions there):

| Scope | Unassisted: one engineer who knows the codebase | AI-assisted, as this repository is built | Calendar, one engineer |
|---|---|---|---|
| macOS only | **23 engineer-weeks** (range 19–27) | ≈ 11 (9–13) | ≈ 5½ months unassisted · ≈ 2½–3 months assisted |
| Linux only | **27 engineer-weeks** (22–31) | ≈ 13½ (11–16) | ≈ 6½ months · ≈ 3–3½ months |
| Both | **38 engineer-weeks** (32–44) | ≈ 19 (16–22) | ≈ 9 months · ≈ 4½ months (two engineers after the shared phases: ≈ 25 / ≈ 12 calendar weeks) |

The first usable macOS builds come early: **M0** a macOS TUI client of the
existing Windows service over the Dev Tunnel (≈ 2 weeks, ≈ 1 assisted),
**M1** a self-contained macOS app with the local service and the TUI
(≈ 8½ weeks, ≈ 3½–4 assisted), **M2** the GPU client on it (≈ 12½ weeks,
≈ 5 assisted). Linux shares about 60 % of the work.

What the research found:

- **The core is already portable.** `canvas-common`, `canvas-model`,
  `canvas-termview`, `canvas-push`, `canvas-mobile`, `canvas-mock` and
  `canvas-host` (without Ghostty) type-check on macOS (host) and on
  `x86_64-unknown-linux-gnu` with 0 errors and 0 warnings (§2). The hosted
  terminal (`canvas-host/src/term.rs`) already runs on `portable-pty`, whose
  Unix backend `canvas-mock` already uses on the Mac: un-gating it leaves **one**
  error (`win::kill_tree`, `term.rs:527`). The TUI uses crossterm. The GUI is
  winit + wgpu + glyphon, all three cross-platform. The parity suite drives the
  GUI through in-process test hooks, not OS automation.
- **The Windows surface is bounded:** about 3,000 lines in Windows-only
  modules (`win.rs`, `pipe.rs`, `dpapi.rs`, `auth.rs`, `taskschd.rs` plus the
  Task Scheduler XML in `factory.rs`, three clipboard modules, the GUI's
  `platform.rs` and `webview.rs`) and **358 production lines in 72 files**
  that call Windows APIs or helpers or assume Windows names and paths (§3).
- **Why nothing else builds today:** `windows`, `windows-sys` and
  `webview2-com` are unconditional dependencies of `canvas-agents`,
  `canvas-core`, `canvas-gui` and the root package, and `windows 0.62` pulls
  `windows-future`, which does not compile off Windows; `ghostty-vt-sys`'s
  build script refuses every target but `*-windows-msvc`.
- **The hard parts** are not the terminal or the protocol: they are the HTML
  report view on Linux (WebView2 has no Wayland-embeddable equivalent),
  secret storage UX (Keychain / Secret Service prompts and headless Linux),
  process control without job objects and without the right to raise
  priorities, the macOS keyboard model (Cmd, Option, Ctrl-click), the
  scheduler (Task Scheduler → launchd / systemd) and packaging (notarization,
  AppImage).

Recommended order (D14): macOS first (the owner works on a Mac and the iPhone
tooling is there), client before service (M0 needs no service port), then
Linux on the shared foundation.

## 2. What exists today

### 2.1 Crates and `cargo check` on this Mac

Rust 1.99.1, `aarch64-apple-darwin` host, macOS 26.3.1. Every crate was
checked on its own (`cargo check -p <crate>`; `--all-targets` where it
builds). Linux: `cargo check --target x86_64-unknown-linux-gnu` (target added
with `rustup target add`). There is no Linux C cross-compiler here, so every
Linux check first failed in the `zstd-sys` C build script (and `openssl-sys`
for `canvas-push`/`canvas-mock`); with a stub C compiler and `ar` (they only
create empty objects, so the check is **type-check only, nothing linked**)
the Rust code was checked for real.

| Crate | Lines | Role | Windows dependencies | macOS `cargo check` | Linux (stub C, type-check) |
|---|---|---|---|---|---|
| `canvas-common` | 5,562 | protocol, transports, settings, pairing | `windows-sys` (Windows-only already); `win`, `pipe`, `dpapi`, `auth` gated | ✓ 0 errors, 0 warnings (all targets) | ✓ 0 / 0 |
| `canvas-model` | 10,215 | document, bus, RPC types, status, report sanitizer | `windows-sys` (Windows-only); a few `cfg(windows)` tests | ✓ | ✓ |
| `canvas-termview` | 727 | L2 terminal view | none | ✓ | ✓ |
| `canvas-push` | 466 | APNs | none (`reqwest` + native-tls) | ✓ | ✓ (needs OpenSSL on Linux, §5.9) |
| `canvas-mobile` | 2,838 | iPhone core (UniFFI) | none | ✓ | ✓ |
| `canvas-mock` | 1,460 | macOS stand-in service, real shells in PTYs | none | ✓ | ✓ (its clipboard is `osascript`, macOS-only, a no-op elsewhere) |
| `canvas-host` | 7,161 | L1: PTY terminals, engines, service loop | `server`, `term` gated off Windows | ✓ (no default features; same as `--no-default-features`) · ✗ `--features ghostty`: `ghostty-vt-sys` build script panics (`target aarch64-apple-darwin is not supported`) | ✓ (no Ghostty) |
| `canvas-agents` | 18,448 | L3 backend, agents, bus, automations, remote | `windows` + `windows-sys` unconditional | ✗ 16 errors, all in the dependency `windows-future 0.3.2` (`IMarshal`, `marshaler`, `windows_threading::submit`); the crate itself is never reached | ✗ the same 16 (`windows-future`) |
| `canvas-core` | 24,781 | app state machine shared by TUI and GUI | `windows-sys` unconditional | ✗ 14 errors: `std::os::windows::process::CommandExt` ×3, `canvas_common::{pipe, win, auth}` ×7, `Command::creation_flags` ×4 | ✗ the same 14 |
| `canvas-tui` | 890 | ratatui front end | via `canvas-core` | ✗ 14 (all `canvas-core`'s) | ✗ |
| `canvas-gui` | 8,147 | GPU client | `windows`, `windows-sys`, `webview2-com` unconditional; wgpu `dx12` only | ✗ 16 errors in `windows-future` (then `canvas-core`, `canvas-agents`, its own Win32 code) | ✗ |
| `canvas-corpus` | 2,333 | golden PTY corpus | uses `canvas_host::term`, `canvas_common::win` | ✗ 4: `canvas_host::term` ×3, `canvas_common::win` ×1 | ✗ |
| `ghostty-vt-sys` | 1,244 | libghostty-vt FFI | MSVC-only build script and ABI test | ✗ build script panics (MSVC only) | ✗ |
| `xtask` | 445 | `build-ghostty` | Windows Zig path default | ✓ builds (runtime defaults are Windows paths) | — |
| root `copilot-canvas` | 15,411 | CLI, service entry, self-tests, parity suite | `windows`, `windows-sys` unconditional | ✗ default features: `ghostty-vt-sys` build script · `--no-default-features`: 16 errors in `windows-future` | ✗ |

Error counts are first-layer: rustc stops at name resolution, so they count
missing modules, not every call site. To see past the `windows-future`
failure, a **scratch copy outside the repository** (temp folder, not the
repo) moved `windows`, `windows-sys` and `webview2-com` to
`[target.'cfg(windows)'.dependencies]`:

| Scratch experiment | macOS | Linux (stub C) | Causes |
|---|---|---|---|
| `canvas-agents`, Windows deps target-gated | 27 errors in 12 files | (blocked by the host experiment below) | `canvas_common::{pipe, win, dpapi, auth}`, `canvas_host::{term, server}` (gated), `windows`/`windows_sys` in `taskschd.rs`, `clipboard.rs`, `backend.rs:390`, `factory.rs:644`, `std::os::windows` in `agents/config.rs`, `copilot_cfg.rs`, `git.rs`, `tunnel_access.rs` |
| `canvas-core`, Windows deps target-gated | 23 | 23 | the 14 above plus `windows_sys` in `clipboard.rs`, `links.rs:257`, `visual.rs:215` |
| `canvas-host` with `term` un-gated | **1** (`term.rs:527`, `win::kill_tree`) | — | the PTY code is portable |
| `canvas-host` with `term` and `server` un-gated | 15 | 15 | `pipe` (L45), `win::*` ×10 (priority, job, SID, private files, usage), `auth::write_token`, `windows_sys` priority classes (L1423) |

No Linux-only Rust error was found: the Linux results match macOS exactly.
Linux-only findings are build-time: C build scripts need a C toolchain
(normal on a Linux CI runner) and `reqwest` with native-tls needs OpenSSL
headers and `libssl` at run time.

### 2.2 Portability already done

- `cfg(windows)` gating (IOS-PLAN Phase 1): `canvas-common` (`lib.rs` gates
  `auth`, `dpapi`, `pipe`, `win`; `tcp.rs` has Unix `readable` and stub
  `peer_process`/`listeners`; `remote.rs:71-80` refuses DPAPI secrets
  elsewhere), `canvas-host` (`lib.rs:10-14` gates `server`, `term`),
  `canvas-model` (Windows-path tests only).
- `canvas-mock` runs real shells (`$SHELL -l`) in `portable-pty` PTYs with
  the service's alacritty engine and key encoder, the real protocol, pairing,
  WebSockets, a reports inbox and, with `--real-agents`, the agent CLIs found
  on the Mac: a working precedent for the Unix PTY path and for launching the
  agents by name from `PATH`.
- CI already has a macOS job (`ios` in `.github/workflows/ci.yml`) running
  clippy and the tests of the portable crates.
- The skills the canvas installs for agents (`canvas-agents/src/cli.rs`
  L296-560) contain no shell-specific commands.

## 3. Windows dependencies inventory

### 3.1 Size per crate

Production lines (comments and `#[cfg(test)]` modules excluded) that call a
Windows API or one of the Windows helpers (`win::`, `pipe::`, `dpapi::`,
`write_private`, `kill_tree`, …), or assume Windows names and paths (`.exe`,
`cmd.exe`, PowerShell, Explorer, `LOCALAPPDATA`, `USERNAME`, backslash
normalization, Task Scheduler):

| Crate | Production lines | Files | Test lines | Whole Windows-only modules |
|---|---|---|---|---|
| `canvas-common` | 58 | 7 | 14 | `win.rs` 466, `pipe.rs` 336, `dpapi.rs` 87, `auth.rs` 45 |
| `canvas-host` | 14 | 2 | 10 | — (`server.rs` priority loop ≈ 45 lines) |
| `canvas-agents` | 96 | 22 | 67 | `taskschd.rs` 193, `clipboard.rs` 71, Task Scheduler XML in `factory.rs` ≈ 440 (L426-870) |
| `canvas-core` | 40 | 9 | 8 | `clipboard.rs` 166 |
| `canvas-gui` | 72 | 10 | 0 | `platform.rs` 426, `webview.rs` 597, `clipimage.rs` 127 |
| `canvas-model`, `canvas-tui`, `canvas-corpus`, `xtask` | 7 | 5 | 4 | — |
| root (`src/`) | 71 | 17 | 33 | — (test drivers: ≈ 13,000 lines, most of them portable) |
| **Total** | **358** | **72** | **136** | **≈ 2,950** |

Plus: `ghostty-vt-sys` (MSVC-only build script and ABI test), the CI
workflow (Windows jobs), and the Windows-only `[dependencies]` in four
`Cargo.toml` files.

### 3.2 By subsystem

| Subsystem | Windows today | Where | macOS / Linux (section) |
|---|---|---|---|
| Process liveness, start time, tree, kill | Toolhelp snapshots, `OpenProcess`, `GetProcessTimes`, `TerminateProcess` | `canvas-common/src/win.rs` L44-125, L233-263, L371-414; callers `term.rs:527`, `service/mod.rs` L662-675, L744-754, L796-840, `index.rs:493`, `claude_history.rs:97`, `reports.rs:80-90`, `agents/opencode.rs:167-172`, `agents/config.rs:89`, `copilot_cfg.rs:59`, `probes.rs:126`, `core/tunnel.rs:224-229` | process groups, `killpg`, process table (§5.2) |
| Job objects | service in a kill-on-close job; `KillOnClose` for `devtunnel connect` | `win.rs` L128-190; `server.rs:379`; `core/tunnel.rs:33, 266-285` | PTY hangup, Linux subreaper + cgroup, macOS remembered trees, parent-death watch (§5.2, §5.3) |
| Priority manager | priority classes, re-applied every 2 s | `server.rs` L1420-1465; `win::raise_priority` (`server.rs:371`, `canvas-tui/src/lib.rs:186`, `canvas-gui/src/main.rs:271`) | `PRIO_DARWIN_BG` / cgroup `cpu.weight` / nice (§5.3) |
| Local IPC, single instance | owner-only named pipe (`D:P(A;;GA;;;<SID>)`), `PIPE_REJECT_REMOTE_CLIENTS`, `FILE_FLAG_FIRST_PIPE_INSTANCE`, identification-only client | `pipe.rs`; `paths.rs:35-47` (`\\.\pipe\copilot-canvas-<USERNAME>-…`); `server.rs:45, 372`; `core/conn.rs:362-374, 424`; GUI test hooks `canvas-gui/src/hooks.rs:29-35`; `src/selftest.rs:305-346` | Unix domain socket, peer credentials, `flock` (§5.3) |
| Private files | protected DACL naming only the user's SID | `win.rs:313-358`; token `auth.rs:12`, port and listen-error files `server.rs:528-533`, APNs key `src/remote_cli.rs:166`, Claude MCP config `agents/claude.rs:124-126` | `O_CREAT\|O_EXCL`, mode 0600 in a 0700 folder (§5.3) |
| Secrets at rest | DPAPI user scope with extra entropy | `dpapi.rs`; `canvas-agents/src/remote.rs:180, 201, 258-260` (`clients.json`); `canvas-common/src/remote.rs:71-80`, `src/remote_cli.rs:296` (`servers.json`) | Keychain / Secret Service / key file (§5.4) |
| Loopback TCP checks | `GetExtendedTcpTable` (peer process, listeners), token SID check, `SO_EXCLUSIVEADDRUSE`, `WSAPoll` | `tcp.rs` L52-76, L170-253; `server.rs:565-576`; OpenCode listener check `agents/opencode.rs:160-172`; tunnel listener probe `core/tunnel.rs` | `/proc/net/tcp` (uid column) on Linux, libproc on macOS (`netstat2` covers both) (§5.3) |
| Process spawn flags | `CREATE_NO_WINDOW`, `DETACHED_PROCESS`, `CREATE_NEW_PROCESS_GROUP`, `CREATE_BREAKAWAY_FROM_JOB`, `CREATE_NEW_CONSOLE` | `core/conn.rs:24-26, 665-692`; `core/platform.rs:106-125`; `core/tunnel.rs:278`; `agents/config.rs:22, 100`; `copilot_cfg.rs:12, 50`; `git.rs:9, 23`; `tunnel_access.rs:20, 60`; `src/remote_cli.rs:409` | `setsid`/`process_group(0)`; no-window is a no-op (§5.1) |
| ConPTY | via `portable-pty`'s Windows backend; behaviours the engines compensate for | `term.rs` L1-11, L240-264, L461-536; `emulator/ghostty.rs` L19, L237, L1199; `emulator/inline.rs` L1-7 | `portable-pty` Unix backend (§5.2) |
| Task Scheduler | COM `ITaskService` (list, XML, run, enable, register with the interactive token), XML generation/import/drift, `GetLocalTime` offset | `taskschd.rs`; `factory.rs` L426-870, L496 (`USERDOMAIN\USERNAME`), L639-648; `factory_cli.rs`; `remote tunnel-setup` manifest (`canvas-agents/src/remote.rs`) | launchd, systemd user timers, cron, built-in launcher (§5.5) |
| Automation launcher | an external Python launcher (`pythonw.exe`) named by `automations.yaml` | `factory.rs:20-27, 95-97` (state under `LOCALAPPDATA`), L501-525 | a built-in Rust launcher (§5.5) |
| Clipboard | `CF_UNICODETEXT`; images `PNG`, `CF_DIBV5`, `CF_DIB` (read); `CF_DIB` + `PNG` (write, service side) | `core/clipboard.rs`; `canvas-gui/src/clipimage.rs:13-16`; `canvas-agents/src/clipboard.rs` | NSPasteboard; X11/Wayland selections; OSC 52 in the TUI (§5.6, §5.8) |
| Opening things | `ShellExecuteW` (URLs, `vscode://`), `explorer.exe` (folders, `/select,`), `reg.exe query HKCR\vscode` | `core/links.rs:242-295`; `canvas-agents/src/backend.rs:444-452`; `copilot_cfg.rs:291-295` | `open` / `xdg-open` / portals / FileManager1 (§5.6) |
| Link detection | drive-letter and UNC paths only; `file://` → backslashes | `core/links.rs:50-94, 166-212` | POSIX paths, the service's path style (§5.9) |
| Path handling | `/` → `\`, lower-casing for comparisons, `\`-split globbing | `git.rs:51`, `vscode.rs:97, 167-170`, `agents/opencode_history.rs:502, 780`, `factory.rs:369-377`, `service/mod.rs:846-849`, `canvas-model/src/sync.rs:399` | a `PathStyle` module (D10) |
| System settings | `SPI_GETCLIENTAREAANIMATION` (reduced motion), `SM_REMOTESESSION` (client `auto`), `GetLogicalDrives` (folder browser roots), `COMPUTERNAME`, `USERNAME` | `core/visual.rs:213-220`; `src/main.rs:312-329`; `backend.rs:386-392`; `canvas-common/src/remote.rs:40-42`; `paths.rs:36` | §5.6, §5.9 |
| Executable names | `copilot.exe`, `opencode.exe` (+ npm `.cmd` shim), `claude.exe` (+ desktop-app exclusion), `gh.exe`, `az.cmd`, `devtunnel.exe` (+ WinGet folders), `copilot-canvas(-gui).exe`, batch-script refusal | `agents/copilot.rs:36`, `opencode.rs:29-65`, `claude.rs:28-64`, `agents/mod.rs:154-181`, `service/mod.rs:756-780` (`nested_copilots` matches `copilot.exe`), `canvas-common/src/remote.rs:158-177`, `cli.rs:269-286`, `core/conn.rs:633-653`, `core/platform.rs:77`, `canvas-gui/src/main.rs:124-134`, `src/main.rs:313-315, 373` | `EXE_SUFFIX`, argv matching, login-shell `PATH` (§5.10) |
| Environment | `PATH` joined with `;` | `service/mod.rs:212` | `std::env::join_paths` |
| Shells | PowerShell demo shells and test shells; `cmd.exe` in tests | `canvas-model/src/doc.rs:1089`, `core/demo.rs:8`, `core/actions.rs:1151`, `term.rs` tests L1357-1430, `src/parity.rs:649`, `src/selftest.rs:1892` | `$SHELL` / `bash --norc` (§5.2, §7) |
| GUI renderer | wgpu with `dx12` only; WARP fallback | workspace `Cargo.toml:70`; `canvas-gui/src/gpu.rs:123-150` | Metal; Vulkan + GL (§5.6) |
| GUI fonts | named Windows font files from `%WINDIR%\Fonts` and per-user fonts | `canvas-gui/src/text.rs:15-78` | Core Text paths / fontconfig + bundled Cascadia Mono (§5.6) |
| GUI shell | icons from resource 1 (`embed-resource`), `HWND` (`hwnd_of`), `GetCursorPos`, quiet test window flags, `WM_KEYDOWN` injection | `build.rs`, `crates/canvas-gui/build.rs`; `shell.rs:281-291, 1567-1602` | §5.6, §7 |
| Taskbar, flash, toasts | `ITaskbarList3`, `FlashWindowEx`, WinRT `ToastNotificationManager` with an AppUserModelID registered in `HKCU`, `GetForegroundWindow`, Action Center history | `canvas-gui/src/platform.rs:103-123, 252-395` | Dock tile, UserNotifications, `LauncherEntry`, freedesktop notifications (§5.6) |
| Monitors, counters | `GetMonitorInfoW` work areas; `GetProcessMemoryInfo`, `GetProcessTimes` | `canvas-gui/src/placement.rs:89-110`; `perf.rs:155-180`; `win::process_usage` | NSScreen, `_NET_WORKAREA`; `sysinfo` (§5.6) |
| HTML view | WebView2 child window, `PrintWindow` capture | `canvas-gui/src/webview.rs` | WKWebView / WebKit (§5.7) |
| Test drivers | `IUIAutomation` + Invoke, `PrintWindow`, `PostMessageW`, PowerShell `Get-CimInstance` for command lines, `PING.EXE` stand-in, console modes, `GetProcessHandleCount` | `src/hardening.rs:423-470`, `src/selftest.rs:928, 1790, 3160`, `src/fake_copilot.rs:235`, `src/paste_probe.rs:293-330`, `src/kitty_demo.rs:87`, `src/soak.rs:491-492` | §7 |
| Ghostty build | `*-windows-msvc` only, `ntdll`/`kernel32`, `.lib` name, ABI manifest for x86_64-windows-msvc, Windows Zig path | `crates/ghostty-vt-sys/build.rs:34-37, 69-72`; `lib.rs:159`; `crates/xtask/src/ghostty.rs:20-23` | §5.2 |

Asked about but **not used** by the code base: SEH or minidumps (only a
panic hook, `canvas-gui/src/crashlog.rs`, and the TUI's in
`canvas-tui/src/lib.rs`), a single-instance mutex (the pipe's first-instance
flag does it), DWM, Direct2D, DirectWrite, D3D directly (wgpu draws, swash
rasterizes glyphs on the CPU), `ImmGetCompositionString` (winit's IME
events), `SetWindowLong`, a `GetMessageW` loop (winit), `schtasks.exe` (the
COM API), the registry beyond the AppUserModelID and the `vscode` handler
query, and WSL.

## 4. Architecture

```
 front ends            canvas-tui (crossterm)        canvas-gui (winit + wgpu + glyphon)
                         │                             ├ os/windows  (today's platform.rs, webview.rs …)
                         │                             ├ os/macos    (AppKit: Dock, menus, WKWebView, pasteboard)
                         │                             └ os/linux    (X11/Wayland, D-Bus, WebKit, fontconfig)
 core                  canvas-core  (Platform trait, KeyPolicy, links/clipboard via canvas-sys)
 backend / service     canvas-agents (Scheduler trait: TaskScheduler | Launchd | SystemdUser | Cron)
                       canvas-host   (term.rs on portable-pty; server on canvas-sys::ipc)
 platform layer  NEW   canvas-sys   process · ipc · fs · secret · spawn · host · clipboard · open
                         ├ windows: re-exports canvas-common::{win, pipe, dpapi, auth} unchanged
                         ├ macos:   libproc, Keychain, AppKit pasteboard, launchd helpers
                         └ linux:   /proc, prctl, cgroups v2, Secret Service, X11/Wayland
 portable (iOS too)    canvas-common · canvas-model · canvas-termview · canvas-push · canvas-mobile
```

### D1. One workspace; a platform crate; Windows code unchanged

- A new crate **`crates/canvas-sys`** holds every operating-system call that
  is not GUI-specific. Its Windows implementation **re-exports** today's
  `canvas_common::{win, pipe, dpapi, auth}` functions: those files are not
  edited, so Windows behaviour cannot change by accident. Call sites move from
  `canvas_common::win::kill_tree` to `canvas_sys::process::kill_tree` (a
  mechanical change, about 70 call sites).
- `canvas-sys` is a separate crate because `canvas-common` also builds for iOS,
  and `cfg(unix)` includes iOS: desktop Unix code (`sysinfo`, Keychain,
  D-Bus) must never reach the iPhone core. `canvas-mobile` does not depend on
  `canvas-sys`.
- **No cfg sprawl.** `cfg` appears only (a) on `mod` declarations in
  `os/mod.rs`-style roots that select `windows.rs` / `macos.rs` / `linux.rs`
  (shared POSIX code in `unix.rs`), (b) in `[target.'cfg(…)'.dependencies]`
  tables, and (c) in `#[cfg(test)]`. `cfg_aliases` in each build script gives
  short names (`macos`, `linux`, `desktop_unix`). Business logic never
  branches on `cfg`; where behaviour must differ at run time it asks
  `canvas_sys::host::platform()` (`Windows | MacOs | Linux`) or, for anything
  about the service's machine, `BackendInfo::platform` (D10).
- Free functions with the same signatures per OS (process, fs, spawn, host)
  are modules; things with state or several backends are traits chosen at
  run time: `Scheduler` (D7), `SecretStore` (D6), `ReportView` (D9),
  `Platform` (exists in `canvas-core/src/platform.rs`), `Notifier`, `Taskbar`.
- Every PR keeps Windows green: the Windows CI jobs stay as they are, and a
  `cargo xwin clippy --workspace --target x86_64-pc-windows-msvc` step (the
  tool IOS-STATUS already uses) runs on the macOS and Linux jobs.

### D2. The same GUI stack on every OS

Keep **winit + wgpu + glyphon**; no native AppKit or GTK rewrite. The GPU
spike chose this stack for its latency (docs/GPU-SPIKE.md §1); all three are
cross-platform; 90 % of `canvas-gui` (scene, quads, text, tiles, canvas,
effects, images, hooks, access) has no OS code; one renderer means one set of
parity checks and comparable golden screenshots. wgpu backends: **Metal** on
macOS, **Vulkan** with a **GL** fallback on Linux (llvmpipe / lavapipe are the
"WARP" there). Native pieces are added at the edges only: the macOS menu bar
and Dock tile, notifications, the HTML view, the pasteboard.

### D3. Terminals on `portable-pty`'s Unix backend

`term.rs` already spawns through `portable-pty` (`native_pty_system`), which
on Unix uses `openpty`, `setsid` and `TIOCSCTTY`: each terminal is its own
session and process group. The code change is the kill path; the
behaviours ConPTY provided (resize repaint, Ctrl+C events, the job object)
map as in §5.2. Both engines work on Unix; Ghostty needs its static library
built with Zig for `aarch64`/`x86_64` macOS and Linux.

### D4. Local IPC: Unix domain sockets, same handshake

The named pipe becomes a Unix socket at `<home>/run/<12 hex>.sock` (falling
back to `$XDG_RUNTIME_DIR` or `$TMPDIR` when the path would pass 100 bytes:
`sun_path` is 104 bytes on macOS, 108 on Linux). The folder is 0700, the
socket 0600, every accepted peer's uid is checked (`SO_PEERCRED` on Linux,
`getpeereid` + `LOCAL_PEERPID` on macOS), and the existing HMAC token
handshake runs unchanged on top. One service per instance: an exclusive
`flock` on `service-<instance>.lock`, held for the process's life, before the
socket is (re)bound. Framing, protocol, heartbeats and reconnect are the
transport-neutral code that already exists.

### D5. Service lifecycle like Windows, plus optional login start

Clients start the service on demand exactly as today (a per-build copy in
`<home>/bin`, upgrade with resume). On Linux the client uses `systemd-run
--user --collect --unit copilot-canvas-<instance>` when a user systemd is
running (the service then lives in its own cgroup, outside the terminal tab
that started it), else a `setsid` double fork; on macOS a `setsid` spawn.
`copilot-canvas service install|uninstall` (new, both OSes) adds a launchd
LaunchAgent or a systemd user unit to start it at login, the equivalent of
the `CopilotCanvas-Service` logon task. The data home stays
**`~/.copilot-canvas`** on every OS (agents' skills, docs, remote paths and
the other agents' own dot-folders all assume it); `COPILOT_CANVAS_HOME`
still overrides it.

### D6. Secrets: envelope sealing with a per-home master key

Windows keeps DPAPI and its hex blobs. On macOS and Linux one random 32-byte
**master key per data home** is kept in the platform store (macOS Keychain
generic password; Linux Secret Service through `oo7` or `secret-service`;
a 0600 key file when no keyring is reachable, which `doctor` reports), and
each secret in `clients.json` / `servers.json` is sealed with it using the
repository's own HMAC-SHA256 stream-and-tag sealing (`canvas_common::pairing`)
and stored as hex with a `v1u:` prefix. One keychain item means at most one
access prompt per binary instead of one per secret, and the JSON files keep
their shape.

### D7. Automations: a `Scheduler` trait with four backends

The manifest stays the source of truth and its model (`canvas_model::automation`)
is unchanged. `Scheduler` = `list`, `definition`, `run`, `set_enabled`,
`register`, `delete`, plus `render(manifest) → definition` and
`import(definition) → manifest` for drift and `automations import`.
Backends: **TaskScheduler** (today's `taskschd.rs` + factory XML, unchanged),
**Launchd** (user LaunchAgents), **SystemdUser** (service + timer units),
**Cron** (fallback when no user systemd). On macOS and Linux runs go
through a **built-in launcher** (`copilot-canvas automations launch NAME`)
that writes the same log lines and environment as the Python launcher, so the
watcher, doctor, health page and canvas boxes need no change.

### D8. Keyboard: Cmd is the app modifier on macOS; Linux keeps the Windows map

`canvas-core` gains a `KeyPolicy` (primary modifier, Ctrl pass-through,
Option as Meta). **macOS GUI:** every app shortcut that is Ctrl+… on Windows
is **Cmd+…** (undo, redo, copy, paste, quit, new window, recent sessions;
Alt+1…9 become Cmd+1…9 because Option+digit types characters); every Ctrl
combination goes to the terminal program untouched (Ctrl+C is always the
program's, as in Terminal.app and iTerm2); Cmd+click opens links and toggles
selection (Ctrl-click is the macOS right click); left Option is Meta (Alt)
for terminals, right Option composes characters. Ctrl+] and F6 still leave a
terminal. **Linux GUI and every TUI:** the Windows map, unchanged.

### D9. HTML reports: native WebKit on both, composed differently

**macOS:** a `WKWebView` child view over the viewer's content area, created
with `objc2-web-kit` and locked down like the iPhone's `ReportWebView.swift`
(JavaScript off, the same content rule list, navigation policy, no new
windows, non-persistent data store), plus a denied permission delegate.
**Linux:** WebKit rendered **offscreen** (WPE WebKit) and drawn as a texture
in the GPU scene, which works on Wayland and X11 alike; decided after a
1-week spike against two alternatives (a WebKitGTK child on X11 with the GUI
forced to XWayland; the Blitz HTML/CSS renderer, which has no script engine at
all). The formatted-text fallback the TUI uses stays for both, as on Windows
without the WebView2 runtime. `wry` is not used: it exposes neither JavaScript
off nor content blocking nor permission denial.

### D10. Path semantics belong to the service's platform

`BackendInfo` gains `platform` (`windows`, `macos`, `linux`; JSON with a
default, so no protocol bump) and `canvas-model` a `PathStyle` (absolute
test, split, join, compare, display, `file://` and `vscode://` mapping). Link
detection, the folder browser, remote paths, VS Code tunnel URIs, drop
quoting and upload names use the **service's** style; mixed-OS remote mode
works both ways (a Mac at home on a Windows Cloud PC; a Windows home PC on a
Linux VM).

### D11. Tests run through the same in-process hooks

The test hooks keep their JSON commands over a Unix socket; offscreen `png`
captures, the `tree`, `screen` and `platform` commands are already
OS-neutral. OS automation is only needed for two measurements (real key
injection for latency, the accessibility tree read from outside), and both
have in-process substitutes for CI. Golden screenshots are kept per OS with
pinned (bundled) fonts.

### D12. Distribution

macOS: a notarized, hardened-runtime **universal** `Copilot Canvas.app` in a
DMG and a Homebrew cask that also links `copilot-canvas` into `PATH`. Linux:
tarball, **AppImage**, `.deb` and `.rpm` for x86_64 and arm64. No App Store
(a terminal host cannot be sandboxed), no Flatpak or Snap in the first
release, no auto-update (the Windows preview has none either).

### D13. Minimum versions

macOS 13 Ventura (`SMAppService`, current WebKit content rules,
UserNotifications; Rust's minimum is 11). Linux: glibc 2.35 (Ubuntu 22.04,
Debian 12, Fedora 39+, RHEL 9 as the floor), X11 and Wayland, systemd optional.

### D14. Order: macOS first, client before service

M0 (macOS TUI client of the Windows service) proves the shared foundation
with real value at once; M1 (local service + TUI) and M2 (GUI) follow; Linux
reuses Phases 0–1 and adds its own desktop integration. §9 has the phases.

## 5. Subsystem designs

### 5.1 The platform layer (`canvas-sys`)

| Module | API (same on every OS) | Windows (existing code) | macOS | Linux |
|---|---|---|---|---|
| `process` | `alive`, `start_ms`, `lock_owner_alive`, `entries` (pid, parent, name, **argv**), `tree`, `kill_tree`, `usage`, `user_of(pid)` | `win.rs` | `sysinfo` (libproc underneath: `proc_pidinfo`, `PROC_PIDTBSDINFO` start time, `KERN_PROCARGS2` argv) | `sysinfo` / `/proc/<pid>/stat` (start time from `btime` + ticks), `/proc/<pid>/cmdline` |
| `process::priority` | `set(pid, Class::{Focused, Visible, Background})`, `raise_self()` | priority classes (`set_priority_class`, `raise_priority`) | `setpriority(PRIO_DARWIN_PROCESS, pid, PRIO_DARWIN_BG)` for background, cleared otherwise; own threads at QoS user-interactive | cgroup v2 `cpu.weight` per terminal when delegated; else autogroup nice (`/proc/<pid>/autogroup`), one-way (§5.3) |
| `process::contain` | `contain_descendants()`, `KillOnClose::new(child)` | job objects | parent-death watcher: the helper is spawned through `copilot-canvas reap --parent PID -- CMD…`, which `kqueue`-waits (`EVFILT_PROC`/`NOTE_EXIT`) for the parent and kills its group | `prctl(PR_SET_PDEATHSIG, SIGTERM)` in `pre_exec`; the service is a child subreaper (`PR_SET_CHILD_SUBREAPER`) |
| `ipc` | `Listener::bind(name)` (fails with `AlreadyExists`), `accept()`, `connect(name, timeout)`, `split()` → reader, writer, closer; `peer() → {pid, uid}` | `pipe.rs` | `UnixListener`, `getpeereid`, `LOCAL_PEERPID` | `UnixListener`, `SO_PEERCRED` |
| `fs` | `create_private`, `write_private` (atomic) | DACL with the user's SID | `O_CREAT\|O_EXCL`, 0600, folder 0700 | the same |
| `secret` | `seal(label, bytes) → String`, `open(String) → bytes`, `backend()` (for `doctor`) | DPAPI hex (unchanged format) | Keychain master key (`security-framework`) | Secret Service master key (`oo7` / `secret-service`) or 0600 key file |
| `spawn` | `CommandExt::no_window()`, `detached()`, `new_console()` | creation flags | no-op · `setsid` · open a terminal window (§5.6) | the same; `systemd-run --user` for the service (D5) |
| `host` | `platform()`, `machine_name()`, `user_name()`, `remote_session()`, `reduced_motion()`, `exe_name("copilot")` | `COMPUTERNAME`, `USERNAME`, `SM_REMOTESESSION`, `SPI_GETCLIENTAREAANIMATION`, `.exe` | `scutil --get ComputerName` (`SCDynamicStoreCopyComputerName`), `getpwuid`, SSH variables, `NSWorkspace.accessibilityDisplayShouldReduceMotion` | `gethostname`, `getpwuid`, SSH variables / no `DISPLAY` and no `WAYLAND_DISPLAY`, the settings portal (`org.gnome.desktop.interface enable-animations`) |
| `net` | `peer_process(local, peer)`, `listeners()` | `GetExtendedTcpTable` (`tcp.rs`) | `netstat2` (libproc) | `netstat2` (sock_diag) or `/proc/net/tcp*` + `/proc/<pid>/fd` |
| `clipboard` | text, has-text, has-image, image as PNG, set image (service side) | three Win32 modules | `NSPasteboard` (objc2-app-kit; TIFF → PNG through `NSBitmapImageRep`) | X11 via `arboard`; Wayland via `smithay-clipboard` on the GUI's own connection; none headless (§5.9) |
| `open` | `url`, `folder`, `reveal(file)`, `with_default_app(file)` | `ShellExecuteW`, `explorer.exe` | `NSWorkspace` / `open`, `open -R` | `xdg-open` for URLs and folders, `org.freedesktop.FileManager1.ShowItems` for reveal, the OpenURI portal when `xdg-open` is missing |

Rules carried over from Windows: nothing is ever executed by opening (on
Unix: files are never passed to `open`/`xdg-open`, where an executable,
`.app` or `.desktop` file could run; folders that are app bundles are
revealed, not opened); every "process tree" walk keeps the start-time guard
against recycled pids.

### 5.2 Terminals

**PTY and processes.** `portable-pty` gives each terminal a new session
(`setsid`) with the slave as controlling terminal, so its process group id is
the shell's or agent's pid. Resizes are `TIOCSWINSZ` and the kernel sends
`SIGWINCH` to the foreground group. The service holds the master side, so a
terminal survives every client, as with ConPTY.

| ConPTY behaviour today | Where | On a Unix PTY |
|---|---|---|
| Resize: ConPTY repaints the viewport itself | `term.rs:431-446`; Ghostty `RESIZE_PULL_SCROLLBACK = false` (`ghostty.rs:237`) | the program redraws on `SIGWINCH`; the engine reflows; use Ghostty's default (pull scrollback on) on Unix, chosen by `host::platform()` |
| Input: legacy key bytes pass ConPTY's input parser, so a few xterm extensions fall back to `keys.rs` | `ghostty.rs:1199` | bytes reach the program directly; keep the `keys.rs` fallback first (identical key tables on every OS), then evaluate Ghostty's own legacy encodings behind the key-table tests |
| Ctrl+C becomes a console `CTRL_C_EVENT` | `term.rs:463` | 0x03 becomes `SIGINT` for the foreground group when the program left raw mode; agents in raw mode read it as a key, as today |
| APC (Kitty graphics) is dropped; OSC 1337 bridged to Kitty placements with the cursor unmoved | `emulator/inline.rs` | APC passes: Kitty graphics from programs work natively with Ghostty; keep the OSC 1337 bridge, but move the cursor below the image as iTerm2 does (no ConPTY cursor to respect) |
| ConPTY waits for a cursor position report at start | emulator tests | not needed; answering is harmless |
| ConPTY re-renders output (its own widths) | GHOSTTY.md "Known differences" | the engine sees the program's own bytes; re-run the corpus and the reviewed differences on Unix recordings |
| Graceful stop: Ctrl+D, Ctrl+D after 2 s, Ctrl+C after 3.5 s, kill after 5 s; the UI-quit-but-lingering rule | `term.rs:461-511` | unchanged; the final kill is `killpg(pgid, SIGHUP)`, 1 s, then `SIGKILL` to the group and to every remembered descendant |
| `kill_tree`: Toolhelp parent links, deepest first | `win.rs:79-117` | the same algorithm over `process::entries`, plus the group kill; on macOS also the pids remembered for that terminal (below) |
| The service's kill-on-close job | `server.rs:379` | the PTY hang-up already sends `SIGHUP` to every terminal session when the service dies; Linux: subreaper + the service's own cgroup (systemd stops what is left); macOS: a sweep at start kills processes recorded (pid + start time) in `<home>/run/terminals.json` by a previous service that died |
| Batch scripts refused (`cmd.exe` re-parses arguments) | `agents/mod.rs:168-181` | not applicable: scripts with a shebang are exec'd directly, never through `sh -c` |

On macOS orphans are re-parented to launchd (no subreaper), so the service
remembers, every 2 s (the priority loop already walks the trees), each
terminal's descendants with their start times; `kill_tree` uses that list
too. That keeps "nothing is orphaned" true for MCP servers that `setsid`
themselves.

**Environment.** A service started by launchd or systemd gets a minimal
`PATH` (no Homebrew, nvm, volta, asdf, mise, `~/.local/bin`) and often no
`LANG`. The service resolves the user's login environment once at start
(`$SHELL -l -i -c 'env -0'` with a 5 s timeout, as VS Code does), merges it
under its own variables, sets `LANG`/`LC_CTYPE` to UTF-8 when unset, and
gives it to every terminal and every CLI it runs. `doctor` shows the
resolved `PATH` and which agents it finds; *Settings › Reload environment*
re-runs it. Plain terminal boxes and the demo run `$SHELL -l` (zsh, bash or
fish) where Windows ran PowerShell; the demo canvas and the tests use
`bash --noprofile --norc` with `PS1='PS> '` so screens stay deterministic.

**Limits.** The service raises `RLIMIT_NOFILE` to its hard limit at start
(launchd's default soft limit is 256; 100 terminals need more). macOS caps
PTYs at `kern.tty.ptmx_max` (511 by default) and processes per user
(`kern.maxprocperuid`, a few thousand): `doctor` reports both, and the
idle auto-stop already frees the ≈ 30 helpers of each idle Copilot session.

**Engines.** alacritty works as is. Ghostty: `cargo xtask build-ghostty
--target <triple>` runs Zig 0.17 with `-Dtarget=aarch64-macos.13.0`,
`x86_64-macos.13.0`, `x86_64-linux-gnu.2.35` or `aarch64-linux-gnu.2.35`
into `target/ghostty/<commit>/<triple>/`; `ghostty-vt-sys/build.rs` accepts
those targets, links `libghostty-vt.a` (and `c++`/`stdc++` if the archive
needs it, to be found in Phase 0) and keeps the provenance checks; the ABI
manifest test compares against a manifest per target (LP64 differs from
Windows' LLP64 wherever a `long` appears). Universal macOS binaries combine
the two Mac archives with `lipo`. Ghostty itself is a macOS and Linux
project first, so the library is expected to build; it was not built here
(no Zig on this Mac, §12).

### 5.3 Service lifecycle and local IPC

| Concern | Windows | macOS | Linux |
|---|---|---|---|
| Endpoint | `\\.\pipe\copilot-canvas-<user>-<instance>-<hash>` | `<home>/run/<hash>.sock`, else `$TMPDIR/cc-<hash>.sock` (per-user, 0700) | `<home>/run/<hash>.sock`, else `$XDG_RUNTIME_DIR/copilot-canvas/<hash>.sock` |
| Who may connect | DACL: the user's SID; local clients only | 0700 folder, 0600 socket, `getpeereid` uid = ours | 0700 folder, 0600 socket, `SO_PEERCRED` uid = ours |
| Client process (logs, "a command in canvas terminal …") | `GetNamedPipeClientProcessId` | `LOCAL_PEERPID` | `SO_PEERCRED` pid |
| Single instance | `FILE_FLAG_FIRST_PIPE_INSTANCE` | `flock(service-<instance>.lock)`, then unlink a stale socket and bind | the same |
| Token and port files | protected DACL | `fs::write_private` (0600) | the same |
| Loopback TCP token check (`--remote loopback`) | peer pid via `GetExtendedTcpTable`, its token's SID | peer pid via libproc (`netstat2`), its uid | the socket's uid straight from `/proc/net/tcp` |
| `SO_EXCLUSIVEADDRUSE` | yes | not needed: no `SO_REUSEADDR`/`SO_REUSEPORT` is set (socket2 sets none), and a wildcard bind does not capture `127.0.0.1` connections | the same (Linux also requires the same uid for `SO_REUSEPORT`) |
| Start | detached, new process group, breakaway from the job | `posix_spawn` with `POSIX_SPAWN_SETSID`, stdio to `/dev/null` | `systemd-run --user …` when available, else `setsid` double fork |
| Per-build copy | `<home>/bin/copilot-canvas-service-<build>.exe` | `<home>/bin/copilot-canvas-service-<build>` (code signature preserved; the `com.apple.quarantine` attribute removed from the copy) | the same; required for the AppImage, whose mount disappears with the launching process |
| Start at login (`service install`) | a Task Scheduler logon task (manifest example) | `~/Library/LaunchAgents/<id>.service.plist` (`RunAtLoad`, `ProcessType=Interactive`, `LimitLoadToSessionType=Aqua`), registered with `launchctl bootstrap gui/$UID`; the bundled app can use `SMAppService.agent` | `~/.config/systemd/user/copilot-canvas.service` (`WantedBy=default.target`, `KillMode=control-group`); `loginctl enable-linger` documented for servers |
| Logs | `<home>/service-<instance>.log` | the same file; launchd's stdout/stderr to `~/Library/Logs/Copilot Canvas/` | the same file; the journal when run by systemd |
| Crash logs | panic hook → `<home>/crash.log` | the same; native crashes in `~/Library/Logs/DiagnosticReports` | the same; native crashes via `systemd-coredump` |
| Idle exit | 10 minutes, no clients, no terminals | unchanged (a launchd agent without `KeepAlive` is simply not restarted) | unchanged (`Restart=no`) |

**Priority manager** (README "The session you type in comes first"). Unix
does not let an unprivileged process raise a priority, so the policy becomes
"lower the others":

- **macOS:** background sessions' process trees get `PRIO_DARWIN_BG`
  (reversible by the same user; it also throttles their disk I/O and keeps
  them on efficiency cores), visible and focused ones are cleared; the
  service's pump and input threads run at QoS user-interactive.
- **Linux:** when the service runs in a delegated cgroup (D5), each terminal
  gets a child cgroup whose `cpu.weight` is 400 (focused), 100 (visible) or
  25 (background); new children inherit it, so the 2 s re-apply only moves
  the class. Without delegation, each PTY session already is its own
  autogroup (fair shares per terminal); background terminals get autogroup
  nice 5 and stay there (unprivileged users cannot lower it again), which
  `doctor` explains.
- `bench --burn 40` decides the defaults per OS in Phase 1;
  `COPILOT_CANVAS_NO_PRIORITY=1` still turns it off.

**TCC on macOS.** Agents read and write the user's projects; when those live
in `~/Documents`, `~/Desktop` or `~/Downloads`, macOS asks once per folder,
attributed to the "responsible" app of the service. Starting the service
through launchd from the bundled app makes that the same app every time
(a service started from a Terminal tab is attributed to Terminal). The
bundle carries `NSDocumentsFolderUsageDescription` and its siblings with a
clear reason.

### 5.4 Secrets

What is stored today, and where it goes:

| Item | File | Windows | macOS / Linux |
|---|---|---|---|
| Paired clients' secrets and revocation keys (service) | `<home>/remote/clients.json` | DPAPI hex | sealed with the master key (`v1u:` hex) |
| A paired server's client secret (client) | `<home>/remote/servers.json` | DPAPI hex | the same |
| Per-start service token | `service-<instance>.token` | owner-only DACL, plain hex | 0600, plain hex (it is per start; the pipe's protection is the folder) |
| Port and listen-error files | `service-<instance>.port`, `.listen-error` | owner-only DACL | 0600 |
| APNs key | `<home>/remote/apns.json` | owner-only DACL | 0600 (optionally sealed) |
| Claude Code MCP config without some servers | private file | owner-only DACL | 0600 |
| OpenCode server passwords, tunnel connect tokens | memory | — | — |

**Store.** macOS: a generic-password Keychain item (service "Copilot
Canvas", account = the data home's hash, `kSecAttrAccessibleAfterFirstUnlock`)
created with an access list naming both executables of the bundle, so the
GUI, the TUI and the service read it without prompts once signed with the
same Developer ID (a per-build copy keeps its signature). Linux: one Secret
Service item (attributes `application=copilot-canvas`, `home=<hash>`); on a
headless machine (no session bus or no keyring) a 0600 key file
`<home>/remote/master.key`, as SSH keys are protected, reported by `doctor`.
`COPILOT_CANVAS_SECRETS=file` forces the file (CI, development builds).
Copying `servers.json` to another machine does not copy its secrets, as with
DPAPI.

### 5.5 Automations

| Manifest field | Task Scheduler (today) | launchd (macOS) | systemd user (Linux) | cron (fallback) |
|---|---|---|---|---|
| `schedule: daily HH:MM` | `CalendarTrigger`, start boundary with the current UTC offset (drifts after DST until applied again) | `StartCalendarInterval {Hour, Minute}` (local time, no drift) | `OnCalendar=*-*-* HH:MM:00` (local) | `M H * * *` |
| `weekly mon,fri HH:MM` | weekly trigger | one `StartCalendarInterval` dict per weekday | `OnCalendar=Mon,Fri *-*-* HH:MM:00` | `M H * * 1,5` |
| `every 2h [from HH:MM]` | repetition | `StartInterval=7200`; with `from`, one calendar entry per hour slot | `OnCalendar=*-*-* 06/2:MM:00` | `M 6-23/2 * * *` |
| `logon` | `LogonTrigger` | `RunAtLoad` | `WantedBy=default.target` on the service | `@reboot` (approximation) |
| `time_limit` | `ExecutionTimeLimit` | the launcher | `RuntimeMaxSec=` | the launcher |
| `overlap: ignore\|queue\|parallel\|stop` | `MultipleInstancesPolicy` | launchd never starts a running job (= ignore); the launcher's lock file does queue/parallel/stop | a timer never starts an active service (= ignore); the launcher for the rest | the launcher |
| `restart_on_failure: N every D` | restart settings | the launcher | `Restart=on-failure`, `RestartSec=`, `StartLimitBurst=` | the launcher |
| `start_when_available` | the setting | missed calendar runs fire on wake (coalesced) | `Persistent=true` | none |
| `success_codes` | launcher / doctor | the launcher | `SuccessExitStatus=` | the launcher |
| "only while signed in" | interactive token | LaunchAgents run in the user's session | user units run with the user manager (also without login when lingering: documented) | as the user |
| Run now | `IRegisteredTask::Run` | `launchctl kickstart gui/$UID/<label>` | `systemctl --user start <unit>.service` | the launcher directly |
| Enable / disable | `SetEnabled` | `launchctl enable\|disable` + `bootstrap\|bootout` | `systemctl --user enable\|disable --now <unit>.timer` | comment the line out |
| State, last run, last result, next run | task properties | `launchctl print gui/$UID/<label>` (state, last exit code); last run and next run from the launcher log and the schedule | `systemctl --user show` (`NextElapseUSecRealtime`, `LastTriggerUSec`, `ExecMainStatus`) | the launcher log and the schedule |
| Where definitions live | Task Scheduler root folder | `~/Library/LaunchAgents/com.copilot-canvas.task.<name>.plist` | `~/.config/systemd/user/copilot-canvas-task-<name>.{service,timer}` | a marked block in `crontab -l` |

- **Launcher.** `copilot-canvas automations launch <state>/tasks/<name>.json`
  does what the Python launcher does: appends `Starting task.` and `Exited
  with code N.` (and launch failures) to `<state>/logs/<name>.log`, sets
  `AUTOMATION_NAME`, `AUTOMATION_RUN_ID`, `AUTOMATION_STATUS_FILE`, runs the
  `report` command with `AUTOMATION_EXIT_CODE`, and enforces what the backend
  cannot (time limit, overlap, retries). The default state folder becomes
  `~/Library/Application Support/BackgroundAutomations` /
  `$XDG_STATE_HOME/background-automations`; `automations.yaml`'s `launcher`
  and `python` keys keep working for owners who want their own launcher.
- **Commands.** `import` reads our own plists/units/cron block (and, with
  `--all`, any user LaunchAgent or user timer); `apply` renders, diffs against
  what is installed (the preview), backs the old definition up to
  `<state>/backups/` and installs with `--yes`; drift compares rendered and
  installed definitions field by field, as `factory::drift` does for XML.
  Manifests with Windows commands (`C:\…\pwsh.exe`) are reported as "not for
  this machine" instead of installed.
- **Unchanged:** manifests, families, the ◷ boxes, failure pulses, the health
  page, log tails, status reports, the doctor and its events (`factory_watch`
  reads the same logs), Copilot's own `data.db` automations
  (`~/.copilot/data.db`, read-only but for the toggle).
- **Remote:** `remote tunnel-setup` writes the same `CopilotCanvas-Tunnel`
  manifest with the full path of `devtunnel` (Homebrew, `~/bin`,
  `/usr/local/bin`); the service-at-login manifest example becomes
  `service install` (D5).

### 5.6 The GUI

How the Windows GUI is built today (docs/GUI.md): winit 0.30.13 windows;
wgpu 30 on DX12 with Mailbox presentation and its own pacing, WARP as
software fallback; glyphon 0.12 (cosmic-text shaping, swash rasterization on
the CPU, a glyph atlas on the GPU); an instanced SDF quad pipeline; 11
layers; damage and loop frames; winit IME events; AccessKit for UI
Automation; WebView2 as a child window over the report viewer; Win32 shell
calls for the taskbar, flashing, toasts, clipboard images and monitor work
areas; an `embed-resource` icon. There is no Direct2D, DirectWrite or GDI
drawing. Everything above the OS calls carries over.

| Area | macOS | Linux |
|---|---|---|
| Windowing | winit (AppKit); `with_option_as_alt(OnlyLeft)`; `ActivationPolicy::Regular` (Accessory for quiet test runs) | winit, Wayland first, X11 fallback; `app_id`/`WM_CLASS` = the `.desktop` name |
| Renderer | wgpu **Metal**; Immediate (Metal has no Mailbox) with our pacing, Fifo fallback; occluded windows already retry | wgpu **Vulkan**, **GL** fallback; Mailbox where offered (FIFO can block while a Wayland window is hidden); llvmpipe/lavapipe as the software adapter with the same notice |
| Fonts | load named files as on Windows (no full scan): bundled **Cascadia Mono** (OFL; identical metrics to Windows), else SF Mono, Menlo; labels SF Pro; fallbacks Apple Color Emoji (sbix: supported by swash), Apple Symbols, PingFang, Hiragino, Apple SD Gothic Neo; paths resolved through Core Text | bundled Cascadia Mono, else fontconfig `monospace`; labels fontconfig `sans-serif` (Cantarell, Noto Sans, DejaVu); emoji Noto Color Emoji **CBDT** (swash renders CBDT and COLRv0, not COLRv1: prefer the CBDT file); CJK Noto Sans CJK; files found with `fc-match` |
| Keys | D8: Cmd shortcuts; Ctrl to the terminal; left Option = Meta; Cmd+click; Cmd+1…9 | the Windows map; Ctrl+Shift+C/V as in GNOME Terminal; AltGr via xkbcommon |
| IME | winit's `NSTextInputClient` (`Ime::Preedit`/`Commit`, `set_ime_cursor_area`), as on Windows | X11: XIM (IBus, Fcitx); Wayland: `text-input-v3` (winit supports it) |
| DPI | backing scale 1 or 2 (scaled modes render at 2); `ScaleFactorChanged` handled already | integer and fractional (`wp_fractional_scale_v1`) scales; X11 `Xft.dpi`; the whole-pixel cell rule handles any scale |
| Clipboard | `NSPasteboard`: text, PNG, TIFF (converted to PNG) | X11 `CLIPBOARD` (`arboard`); Wayland `wl_data_device` through the GUI's own connection (`smithay-clipboard`: it works on GNOME, which has no data-control protocol) |
| File drag and drop | winit `DroppedFile`; paths quoted for POSIX shells (`'…'`) | X11: winit; **Wayland: winit 0.30 has no drag and drop** (only macOS, X11 and Windows emit `DroppedFile`): handle `wl_data_device` drops on the same connection, or move to winit 0.31 when it lands |
| Menus, quit | a real menu bar (`muda` or objc2-app-kit): App (About, Settings… ⌘,, Hide, Quit ⌘Q), File (New Session, New Window ⌘N), Edit (Undo, Redo, Cut, Copy, Paste, Select All: needed for the web view and text fields), View (zoom, Show all nodes, Tile terminals), Window, Help. ⌘Q opens the same quit dialog (detach or stop all); the red button on the last window detaches; a Dock click reopens a window | no menu bar (as on Windows): the toolbar and the canvas menus; the close button as on Windows |
| Taskbar state | Dock tile: badge = sessions that need you; an overlay on the icon drawn in the status colours (blue busy, gold needs you) | Unity `LauncherEntry` D-Bus signal (`progress`, `count`, `urgent`), shown by KDE Plasma, Ubuntu Dock and Dash to Dock; plain GNOME shows nothing (title and urgency remain) |
| Flash | `NSApp.requestUserAttention(.informationalRequest)` (a Dock bounce); winit's `request_user_attention` does it | X11 urgency hint; Wayland `xdg-activation` (winit's `request_user_attention`) |
| Toasts | `UNUserNotificationCenter` (needs the app bundle and the user's permission; first toast asks); outside a bundle, nothing is shown and the platform records why | `org.freedesktop.Notifications` (`notify-rust`/zbus); the Notification portal inside a sandbox |
| "In front" (no toast) | `NSApp.isActive` and the key window | winit focus |
| Links and files | URLs `NSWorkspace.open`; folders opened in Finder (bundles revealed); files in VS Code (`code -g file:line`, or `vscode://file/…` through LaunchServices) or revealed (`open -R`) | URLs `xdg-open`; folders `FileManager1.ShowFolders`, else `xdg-open DIR`; files `code -g` or `FileManager1.ShowItems` |
| Reduced motion | `accessibilityDisplayShouldReduceMotion` | settings portal / `gsettings org.gnome.desktop.interface enable-animations`; KDE `AnimationDurationFactor` |
| Window placement | positions and sizes; work areas from `NSScreen.visibleFrame` | X11: positions, `_NET_WORKAREA`; **Wayland: no positions** (the compositor places windows): size and maximized only |
| Icons | `.icns` in the bundle; `NSApp.setApplicationIconImage` for unbundled runs | hicolor PNG/SVG icons + `.desktop`; winit `with_window_icon` on X11 |
| Accessibility | AccessKit → NSAccessibility (VoiceOver) | AccessKit → AT-SPI (Orca), `accesskit_unix` over zbus |
| Other client (`O`) | TUI: a `.command` file opened in Terminal (no Apple Events permission needed), or the user's terminal (`tui_terminal` setting: iTerm2, Ghostty, WezTerm, kitty) | `$TERMINAL`, `x-terminal-emulator -e`, `gnome-terminal --`, `ptyxis --`, `konsole -e`, `kitty`, `alacritty -e`, `foot`, `wezterm start --` |
| Several windows | winit; one process; a second `--gui` launch asks the running GUI for a window (its hooks-style socket) instead of a second Dock icon | as on Windows (several processes allowed) |
| Background | App Nap can coalesce the UI thread's timers while hidden: `NSProcessInfo.beginActivity(.userInitiated)` while sessions run or the test hooks are on | — |
| Counters (`--perf`, benches) | `canvas_sys::process::usage` | the same |

`hwnd_of` (`shell.rs:1567`) becomes a small `NativeWindow` value
(HWND / NSView / X11 window / Wayland surface) that the platform modules take;
`cursor_pos` uses winit's last `CursorMoved` instead of `GetCursorPos`.

### 5.7 The HTML report view

The sanitizer (`canvas_model::report_html`) and the Content-Security-Policy
document are shared by every client already. The view's own lock-down per
platform:

| Layer | Windows (WebView2, `webview.rs`) | macOS (`WKWebView`, mirrors `ios/…/ReportWebView.swift`) | Linux (WebKit: WPE offscreen, or WebKitGTK) |
|---|---|---|---|
| Scripts | `IsScriptEnabled = false`, no web messages, no host objects, no script dialogs | `defaultWebpagePreferences.allowsContentJavaScript = false`, `javaScriptCanOpenWindowsAutomatically = false`, no script message handlers | `enable-javascript = false` (and `enable-javascript-markup = false`) |
| Network | every request answered 403 (`WebResourceRequested` for `*`) | content rule list blocking `http(s)`, `ws(s)`, `ftp`, `file`, `blob` (the iPhone's JSON, unchanged) | the same JSON as a `WebKitUserContentFilter` (WebKit's content blockers use the same format); `send-request` handler refusing anything but `about:blank` and `data:` |
| Navigation | everything but the canvas's own `NavigateToString` cancelled; a clicked link becomes a request to the canvas | `decidePolicyFor` allows the page's own `about:blank` load and in-page jumps, cancels the rest, sends clicked `http(s)`/`mailto` links to the canvas | `decide-policy` signal, the same rules |
| New windows, downloads, permissions, external schemes | refused, cancelled, denied, never launched | `createWebViewWith` returns nil; `requestMediaCapturePermissionFor` denies; no downloads (no download delegate) | `create` signal returns nothing; `permission-request` denied; `download-started` cancelled |
| Storage | own user-data folder `<home>/webview2` | `WKWebsiteDataStore.nonPersistent()` | an ephemeral `WebKitNetworkSession` / website data manager |
| Chrome | no devtools, context menu, status bar, zoom, browser keys, autofill | no link previews, no back/forward gestures, media only on user action (the iPhone's data-detector and inline-media switches are iOS-only); no devtools (`isInspectable = false`) | `enable-developer-extras = false`; context menu suppressed |
| Esc closes; the view never takes the keyboard by itself | yes | `keyDown` override for Esc; focus returned to the canvas view on hide | Esc forwarded by the canvas (offscreen: the canvas owns input) |
| Composition | a child HWND over the pane, moved and hidden after each frame | an `NSView` subview of winit's content view, above the Metal layer (layer-backed, as `wry`'s wgpu example does), frame from the pane's cell rectangle (flipped coordinates) | **offscreen**: the page rendered to buffers and drawn as a texture in the scene, input forwarded; or (alternative) a WebKitGTK child on X11 with the GUI on XWayland |
| Test capture | `CapturePreview` (the view), `PrintWindow` (composed window) | `takeSnapshot` (the view); the composed capture is built in-process from the offscreen frame plus the snapshot at the view's real frame | the view is part of the offscreen `png` |
| Missing runtime | formatted text with a note | always present (system WebKit) | formatted text with a note (package dependency otherwise) |

Linux needs the spike because Wayland gives a client no way to place a
foreign toolkit's surface inside its own window, and winit and GTK cannot
share one Wayland surface. The offscreen route keeps one code path for X11
and Wayland and makes the parity capture trivial; its costs are WPE packaging
(Debian/Ubuntu and Fedora ship it; no maintained Rust bindings, so bindgen
over its GObject C API) and input forwarding (scroll, click, selection: the
viewer is read-only). The spike measures fidelity on the golden report,
startup time and memory for (a) WPE offscreen, (b) WebKitGTK as an X11 child
with the GUI forced to XWayland, (c) Blitz (an HTML/CSS renderer without a
script engine or network unless one is given, drawing with wgpu: the
strongest lock-down, the least mature).

### 5.8 The TUI

The TUI is crossterm with raw mode, the alternate screen, SGR mouse,
bracketed paste and focus events (`canvas-tui/src/lib.rs:187-204`); on Unix
those arrive as escape sequences instead of console events, so pastes come as
one `Event::Paste` and the Windows burst detection (`input::coalesce`) is
simply not exercised. Changes: `raise_priority` becomes `priority::raise_self`
(a no-op on Unix); the clipboard is the system one when a desktop session is
present (`DISPLAY`/`WAYLAND_DISPLAY`/Aqua) and **OSC 52** to the outer
terminal otherwise (SSH); 256-colour output when `COLORTERM` is not
`truecolor` (Terminal.app has no 24-bit colour; iTerm2, Ghostty, WezTerm,
kitty, GNOME Terminal and Konsole have). Alt+V, Alt+1…9 need the terminal's
"Option as Meta" on macOS (an ESC prefix is the same key to crossterm); the
help screen says so. OSC 9;4 progress and the bell go to the outer terminal
as today; terminals that do not implement OSC 9;4 ignore it. A window of at
least 140 × 40 is still the comfortable size.

### 5.9 Remote mode, Dev Tunnels, the iPhone pieces

- **Transport, pairing, WebSockets, compression, heartbeats, reconnect** are
  portable today (checked: `canvas-common` and `canvas-mock` build and test
  on macOS; `canvas-mobile/tests/end_to_end.rs` runs against the mock).
- **Dev Tunnels CLI** exists for macOS (Homebrew cask `devtunnel`) and Linux
  (install script to `~/bin`). Discovery: `COPILOT_CANVAS_DEVTUNNEL`, `PATH`,
  then `~/bin`, `/opt/homebrew/bin`, `/usr/local/bin` (replacing the WinGet
  folders in `canvas-common/src/remote.rs:158-177`). The managed
  `devtunnel connect` child is killed with the client by `process::contain`
  (§5.1). Its "is this listener the child's?" check uses `canvas_sys::net`.
  The tunnel host is the `CopilotCanvas-Tunnel` automation (§5.5).
- **Secrets** of pairing on both sides: §5.4. `machine_name` from §5.1.
- **Mixed OS (D10).** A Mac client on the Windows Cloud PC works once the
  client builds: `remote_path` (`core/links.rs:202-212`) accepts only
  Windows paths today, which is right for a Windows service. A Windows client
  on a Linux or Mac service needs `remote_path`, the folder browser
  (`ListEntries` folders end in `/`), upload names (Windows device names only
  matter on a Windows service), drop quoting and VS Code tunnel URIs
  (`vscode://vscode-remote/tunnel+NAME/home/…`) to follow
  `BackendInfo::platform`.
- **Image paste on the service's clipboard** (`Request::SetClipboardImage`):
  macOS writes `NSPasteboard` when the service runs in the user's GUI session
  (launched from the app, Terminal or launchd's Aqua session; not over SSH);
  Linux X11 keeps a thread that owns the selection; Wayland background
  processes cannot set the clipboard (except through data-control protocols
  GNOME lacks); headless Linux has no clipboard. When the clipboard cannot be
  set, the image is written to `<home>/uploads/<date>/` and its quoted path
  is pasted instead (a documented deviation; whether each agent attaches an
  image given as a path is to be verified).
- **APNs** (`canvas-push`) is portable; on Linux `reqwest` with native-tls
  needs OpenSSL: switch the Linux build to rustls (target-specific feature)
  so the AppImage carries no OpenSSL dependency. Tunnel access tokens
  (`tunnel_access.rs`) only lose `CREATE_NO_WINDOW`.
- **Remote session detection** for `client auto`: SSH variables everywhere;
  no display on Linux; on macOS a Screen Sharing session cannot be detected
  reliably, so `auto` there means "GUI unless SSH".

### 5.10 Agents

| | Copilot CLI | OpenCode | Claude Code |
|---|---|---|---|
| Install on macOS/Linux | npm `@github/copilot` (a Node script), or GitHub's installer | Homebrew, the install script (`~/.opencode/bin`), npm `opencode-ai`; a native binary | the native installer (`~/.local/bin/claude`), Homebrew cask, npm `@anthropic-ai/claude-code` |
| Found by | `COPILOT_CANVAS_COPILOT`, else `copilot` on the resolved `PATH` (§5.2) | `COPILOT_CANVAS_OPENCODE`, else `opencode` | `COPILOT_CANVAS_CLAUDE`, else `claude`, `~/.local/bin/claude` |
| History | `~/.copilot/session-state/<id>/events.jsonl`, `inuse.<pid>.lock` (to verify: whether `XDG_CONFIG_HOME` moves `~/.copilot` on Linux) | `~/.local/share/opencode/opencode.db` (already `XDG_DATA_HOME`-aware, `opencode_history.rs:29`), `~/.local/state/opencode/model.json` | `~/.claude/projects/<dir>/<id>.jsonl`, `~/.claude/sessions/<pid>.json`, `~/.claude.json` (`CLAUDE_CONFIG_DIR`) |
| Live checks | lock owner alive with the start-time guard | its HTTP server; the listener-owner check via `canvas_sys::net` | pid file: `procStart` is a Windows FILETIME today (`claude_history.rs:95-101`); its Unix format is unknown, so the existing `startedAt` rule is used until verified |
| Spawned subsessions (`copilot -p` inside a terminal) | `nested_copilots` matches the process name `copilot.exe`; on Unix the process is `node …/copilot`: match argv (the script's path or package name) | — | — |
| Data paths in history | `C:\…` today; POSIX on Unix: every `replace('/', "\\")` goes through `PathStyle` | the same | `project_dir_name` maps every non-alphanumeric character, so it is already portable |

- The agents' own keys on macOS: image paste is Ctrl+V for Claude Code in a
  terminal (Cmd+V is the terminal's). The GUI's Cmd+V with only an image on
  the clipboard sends **Ctrl+V** (0x16) to the agent, as Windows sends the
  paste key; this is to be verified for Copilot CLI and OpenCode.
- **VS Code.** Storage: `dirs::config_dir()` already gives
  `~/Library/Application Support/Code` and `~/.config/Code` (`vscode.rs:71`,
  `migrate.rs:171`); Insiders "Code - Insiders". Linux Snap
  (`~/snap/code/current/.config/Code`), Flatpak
  (`~/.var/app/com.visualstudio.code/config/Code`) and VSCodium are optional
  extra roots. "Is VS Code installed" (`links.rs:242-253`, `reg.exe`)
  becomes: `code` on `PATH`, or a `vscode` URL handler
  (`LSCopyDefaultApplicationURLForURL`; `xdg-mime query default
  x-scheme-handler/vscode`). Files open with `code -g file:line` (no browser
  hop), folders with `code DIR` where VS Code is chosen, tunnel URIs with
  `open`/`xdg-open vscode://vscode-remote/tunnel+…`.
- **`gh` and `az`** for pull request coordination are found on the resolved
  `PATH` (`gh`, `az`, no `.exe`/`.cmd`); stand-ins in tests as on Windows.
- **No WSL assumption.** The Linux port is native Linux (desktop or headless
  server, x86_64 and arm64). WSL2 can host a Linux service (TUI, remote mode)
  but is not a target for the GUI; the Windows app never needs WSL.

## 6. Parity matrix

Every row of [PARITY.md](PARITY.md) (row ids as there; 60 rows). "Core"
means the row is implemented in `canvas-core`, `canvas-model`, the service or
the renderer and only needs the platform layer underneath; its check moves to
macOS and Linux with the parity suite (§7). Difficulty S/M/L is the
OS-specific work for that row; risk L/M/H is the chance it cannot be matched
exactly or needs rework. Every row applies to the TUI as well where PARITY.md
says so; the TUI needs no OS work beyond §5.8.

| # | Feature | macOS | Linux | Platform API | Diff mac / lin | Risk |
|---|---|---|---|---|---|---|
| 1 | Canvas: drag, pan, zoom, Show all nodes, centre, double-click, menus | core; trackpad pixel scrolling accumulates into notches; Cmd/Option+wheel zoom alongside Ctrl/Alt (Ctrl+scroll can be the system zoom); pinch zoom added | core; libinput smooth scrolling | winit `MouseWheel` (`PixelDelta`), `PinchGesture` | S / S | L |
| 2 | Status (seven states), OSC 9;4, background subagents, Esc not done | core + history readers on Unix paths; verify each agent's files on macOS/Linux | same | file system, PTY | M / M | M |
| 3 | Attention borders and tints; halo | renderer | renderer | wgpu | S / S | L |
| 4 | Colour-blind palette; icons ◆ ✓ ○ ■ ✗ ↗ | renderer; glyph coverage from SF/Apple Symbols or the bundled fonts | renderer; DejaVu/Noto or bundled | fonts | S / S | L |
| 5 | Subagents from history | core | core | — | S / S | L |
| 6 | Subsessions (`S`, fork, dotted); spawned-subsession detection | fork is file copying (portable); detection by argv (`node …/copilot`) | same | `canvas_sys::process::entries` | M / M | M |
| 7 | Loops: arc, `↻ every`, sidebar ↻ | renderer | renderer | — | S / S | L |
| 7b | Smooth loop dot; reduced motion | renderer; reduced motion from `NSWorkspace` | renderer; settings portal / gsettings | `host::reduced_motion` | S / S | L |
| 8 | MCP picker with presets | core + agent CLIs | same | — | S / S | L |
| 9 | Groups, chips, launch defaults | core; folders `~/.copilot-canvas/groups/…`; open in Finder | same; file manager | `canvas_sys::open` | S / S | L |
| 10 | Sidebar tree, filter, fly-to, group rows | core | core | — | S / S | L |
| 11 | Terminals: PTY, single or 4 tiles, survive client exit | `portable-pty` Unix; UDS service | same | `openpty`, `setsid`, UDS | M / M | M |
| 11b | Detach and reattach | service started with `setsid` | `systemd-run --user` or `setsid` | — | S / S | M |
| 12 | Wheel rules (mouse mode, PgUp/PgDn, scrollback, Shift) | core; trackpad momentum accumulation | core | winit | S / S | L |
| 13 | User-message tint | renderer | renderer | — | S / S | L |
| 14 | Copy and paste (drag-select, copy on release, copy/paste keys, pane menu; a paste never sends) | Cmd+C (with selection), Cmd+V; Ctrl+C always to the program; `NSPasteboard`; paste into zsh/bash runs nothing (bracketed paste) | Ctrl+C with selection, Ctrl+Shift+C/V, Ctrl+V; X11/Wayland clipboards | `canvas_sys::clipboard` | M / M | M |
| 15 | Image paste; thumbnail | Cmd+V with only an image → Ctrl+V to the agent; PNG/TIFF from the pasteboard | Ctrl+V/Alt+V; `image/png` from the selection | pasteboard / selections | M / M | H (agents' image keys on Unix unverified) |
| 16 | Click forwarding in mouse mode; drags select | core | core | — | S / S | L |
| 17 | Title, taskbar state mirror, bell, OSC 777 | title; Dock badge + icon overlay; `requestUserAttention`; `UNUserNotificationCenter` | title; `LauncherEntry`; urgency / xdg-activation; freedesktop notifications | AppKit, UserNotifications, D-Bus | M / M | M (GNOME has no progress) |
| 18 | Links: URLs, OSC 8, paths; Ctrl+click; nothing executed | POSIX and `~/` paths; **Cmd+click** in terminals; `NSWorkspace`, `code -g`, `open -R`; never `open` a file | POSIX paths; Ctrl+click; `xdg-open` for URLs and folders only; `code -g`; FileManager1 | `canvas_sys::open`, `PathStyle` | M / M | M |
| 19 | New session dialog: folder browser, Recent, Git, worktree, model, MCP, agent | core; browser roots `/`, `~`, `/Volumes/*` | roots `/`, `~`, `/media/$USER/*`, `/mnt/*` | `ListDir` | S / S | L |
| 20 | Import; VS Code sync (+ undo, auto sync) | VS Code storage under `~/Library/Application Support/Code` | `~/.config/Code` (+ Snap/Flatpak roots optional) | `dirs::config_dir` | S / S | M |
| 21 | Activity bar views | **Cmd+1…9** (Option+digit types characters) | Alt+1…9 | `KeyPolicy` | S / S | L |
| 22 | Copilot defaults, group overrides, `copilot config` | CLI on the resolved `PATH` | same | — | S / S | L |
| 23 | Plugins, MCP, skills; install in a terminal | same | same | — | S / S | L |
| 24 | Migrate from VS Code (preview, backup, redacted log) | config paths per OS | same | `dirs` | S / S | M |
| 25 | Passive sessions (templates, panel, runs, budgets) | headless runs with `no_window()` (a no-op) | same | — | S / S | L |
| 26 | Automations: families, failure pulse, health page, run, enable, doctor, outputs | Launchd backend + launcher | SystemdUser / Cron backends + launcher | `launchctl`, `systemctl --user`, `crontab` | L / L | M |
| 27 | Copilot automations (`data.db`), session schedules | `~/.copilot/data.db` read-only (SQLite URI with percent-encoded path) | same | rusqlite | S / S | L |
| 28 | Idle auto-stop, priority manager, upgrade with resume, protocol tolerance, crash log | `PRIO_DARWIN_BG`; per-build copy without quarantine; panic hook | cgroup `cpu.weight` / autogroup; per-build copy (AppImage) | `canvas_sys::process::priority` | M / M | M (priorities cannot be raised) |
| 29 | Keyboard map, F6 / Ctrl+], quit dialog, help | Cmd+Q and the menu bar open the quit dialog; F6 needs Fn on laptop keyboards (Ctrl+] remains) | the Windows map | `KeyPolicy`, menus | M / S | M |
| 30 | `--demo` | demo shells `bash --noprofile --norc` | same | — | S / S | L |
| 31 | `--perf` overlay | counters from `canvas_sys::process::usage` | same | `sysinfo` | S / S | L |
| 32 | CLI | the same binary; no `.exe` names; `<home>/cli` on `PATH` with `:` | same | `join_paths` | S / S | L |
| 33 | Remote mode (pair, reconnect, resync, offline input, upload) | `devtunnel` for macOS; Keychain secrets; uploads with the service's path style | `devtunnel` for Linux; Secret Service / key file | §5.4, §5.9 | M / M | M |
| 34 | View versus start | core | core | — | S / S | L |
| 35 | Transcript inspector; child resume | core | core | — | S / S | L |
| 36 | Removing a multi-selection | core; Cmd+Z restores | core | — | S / S | L |
| 37 | Group background and border selection | core | core | — | S / S | L |
| 38 | Recent sessions; Ctrl+F6 / Ctrl+Shift+F6, `]` / `[` | Cmd+F6 and Ctrl+F6 (F-keys need Fn), `]`/`[` | the Windows keys | `KeyPolicy` | S / S | L |
| 39 | Default model per agent | core | core | — | S / S | L |
| 39b | Remembered model after restart | core | core | — | S / S | L |
| 40 | Canvas-wide agents: panel, frame, sidebar | core | core | — | S / S | L |
| 40b | Canvas-wide agent runs across groups | service | service | — | S / S | L |
| 41 | Reports inbox, unread count, toast | toast through UserNotifications | freedesktop notification | §5.6 | S / S | M |
| 41b | HTML reports, locked down | `WKWebView` child (§5.7) | WebKit offscreen or the spike's choice (§5.7) | objc2-web-kit · WPE / WebKitGTK | M / L | M / **H** |
| 41c | Remote reports | core | core | — | S / S | L |
| 42 | Pull request coordination (`gh`/`az`) | CLIs on the resolved `PATH` | same | — | S / S | M (environment) |
| 43 | Review follow-ups | service | service | — | S / S | L |
| 44 | Undo/redo of layout changes | **Cmd+Z, Cmd+Shift+Z** (Cmd+Y kept); in a terminal Ctrl+Z still reaches the program | Ctrl+Z, Ctrl+Y, Ctrl+Shift+Z | `KeyPolicy` | S / S | L |
| 45 | Area selection of subsessions and subagents | Shift-drag, right-drag; **Cmd-click** toggles (Ctrl-click is a right click on macOS) | as Windows | `KeyPolicy` | S / S | L |
| G1 | Inline images (Kitty, iTerm2) | renderer; Kitty APC now reaches Ghostty directly | same | — | S / S | L |
| G2 | Taskbar progress, flash, toasts | as row 17 | as row 17 | as row 17 | M / M | M |
| G3 | File drag and drop (upload in remote mode) | winit `DroppedFile`; POSIX quoting | X11 winit; **Wayland: own `wl_data_device` handling** | sctk on winit's display | S / M | M |
| G4 | Several windows | Cmd+N (and Cmd+Shift+N) | Ctrl+Shift+N | winit | S / S | L |
| V1 | Dust | renderer | renderer | — | S / S | L |
| V2 | Stacks and hidden subsessions | renderer | renderer | — | S / S | L |
| V3 | Effects | renderer | renderer | — | S / S | L |
| V4 | Group chips at any zoom; sidebar rows | renderer | renderer | — | S / S | L |
| V5 | Remote group files in the canvas | core; the service's path style | same | `PathStyle` | S / S | L |

The hardening checks (GUI.md §3) map too: device loss (Metal and Vulkan
devices can be destroyed the same way), DPI change, window placement
(Wayland: size and maximized only), the accessibility tree (AX / AT-SPI
instead of UI Automation), crash reports, the software renderer (Linux:
llvmpipe/lavapipe; macOS has none, so only the "no adapter" message is
checked), damage frames.

### 6.1 What cannot be 1:1, and the equivalent

| Windows feature | Why not identical | Equivalent |
|---|---|---|
| Taskbar progress bar (busy/paused) | GNOME has no progress API; macOS has no taskbar | macOS: Dock badge (count) + status overlay on the icon; Linux: `LauncherEntry` where the dock supports it (KDE, Ubuntu Dock, Dash to Dock); window title and urgency everywhere |
| Action Center history check | no history API on Linux | macOS: delivered notifications list; Linux: the test harness owns a fake notification server on the session bus and checks the calls |
| Priority manager raising the typed-in session above normal | Unix forbids raising priority without privilege | lower the others: `PRIO_DARWIN_BG` (macOS), cgroup weights or autogroup nice (Linux); measured with `bench --burn` |
| Kill-on-close job object | no job objects | PTY hang-up, Linux subreaper + cgroup, macOS remembered trees + start-up sweep, parent-death watchers for helpers |
| Window placement across monitors | Wayland clients cannot read or set positions | size and maximized state restored; positions on macOS and X11 |
| Ctrl-based shortcuts on macOS | Cmd is the platform's app modifier; Ctrl-click is right click; Option types characters | D8: Cmd for app shortcuts, Ctrl to programs, Cmd+click, Cmd+1…9 |
| Alt+V image paste on macOS | Option+V types `√` | Cmd+V handles images too (sends Ctrl+V to the agent) |
| Service sets the clipboard image for a remote paste | headless Linux has no clipboard; Wayland background processes cannot set one; macOS over SSH has no pasteboard | the image is uploaded and its path pasted |
| Task Scheduler drift after daylight-saving changes | launchd and systemd use local time | nothing to fix: drift is reported only for real differences |
| WebView2 child window | no embeddable web view surface on Wayland | WebKit rendered offscreen into the scene (or the spike's alternative) |
| "Open in the default program" for automation outputs | `open`/`xdg-open` could run an executable or `.desktop` file | only non-executable files are opened; others are revealed |
| `SM_REMOTESESSION` for `client auto` | Screen Sharing cannot be detected reliably | `auto` = GUI unless SSH (or no display on Linux) |
| AppUserModelID taskbar grouping | — | bundle identifier (macOS), `.desktop` file + `app_id` (Linux) |
| Ghostty's ConPTY-specific workarounds | no ConPTY | Unix behaviour per §5.2 (Kitty APC works natively; the OSC 1337 bridge moves the cursor) |

## 7. Testing and CI

### 7.1 The suites on macOS and Linux

| Suite | Today (Windows) | macOS / Linux |
|---|---|---|
| Unit and integration tests (`cargo test --workspace`), ViewModel snapshots | portable except tests that start `cmd.exe`/`powershell.exe` (`term.rs` L1357-1430, `win.rs` tests, `opencode.rs:762, 776`, `core/tunnel.rs:408`) or use Windows paths | those tests get Unix twins (`/bin/sh -c`, `sleep`) behind `cfg_aliases`; Windows-path tests stay Windows-only; new tests for `canvas-sys` (peer credentials, private files, process trees with re-parenting, secrets round trip with the file backend) |
| Golden corpus (`corpus check`) | recordings made through ConPTY | the existing recordings replay on any OS (they are byte streams); new entries recorded through a Unix PTY (`copilot-canvas record`, which already uses `portable-pty`) for zsh, bash, vim, the agents |
| TUI self-test (39 steps) | the TUI in a hidden pseudoconsole (`selftest.rs:20-70`, `portable-pty`) | unchanged driver; demo shells `bash --norc`; stand-ins copied without `.exe` (`EXE_SUFFIX`); `TERM=xterm-256color` set for the child |
| `--remote loopback`, `faults`, `paired`, `soak` | TCP + the `devtunnel-standin` | portable; `soak` reads handle counts from `/proc/<pid>/fd` or libproc instead of `GetProcessHandleCount` |
| Backend contract (`tests/backend_contract.rs`) | real service over TCP | unchanged |
| GUI self-test, parity suite, goldens, hardening, benches | `--test-hooks` over a named pipe | the same JSON commands over a Unix socket (`<home>/run/gui-test-<pid>.sock`, 0600); `GuiDriver` connects there |
| Command-line checks in agent probes | PowerShell `Get-CimInstance Win32_Process` (`selftest.rs:928, 3160`) | `canvas_sys::process::entries` argv (portable, and faster) |
| Stand-in agent and paste probe | console modes (`fake_copilot.rs:235`, `paste_probe.rs:293-330`) | termios raw mode (crossterm) |
| Toast assertions (rows 17, G2) | Action Center history | macOS: delivered notifications (bundled app) or "recorded, not shown" in quiet runs; Linux: a fake `org.freedesktop.Notifications` on a private session bus |
| HTML view (row 41b) | `CapturePreview` and `PrintWindow` | macOS: `takeSnapshot` and an in-process composite at the view's real frame; Linux offscreen: the normal `png` hook; `eval` through the view's own script API (page scripts stay off); a local listener proves no request left the view, as today |
| Accessibility (hardening) | `IUIAutomation` reads the tree and invokes "+ New" | in-process `access` hook everywhere; Linux CI reads the AT-SPI tree over D-Bus and invokes the action; macOS reads `AXUIElement` only on a developer Mac (it needs the Accessibility permission, which CI runners do not grant) |
| Keystroke → present (`post_keys`, `bench --burn`) | `PostMessageW(WM_KEYDOWN)` | macOS: `NSApp.postEvent` of a key-down in-process (goes through AppKit and winit like a real key); X11: XTest; Wayland: in-process synthetic events (documented) |
| Window capture without the screen | offscreen render works on a locked workstation | offscreen render works the same; nothing needs the macOS Screen Recording permission |

**Golden screenshots.** Fonts decide most pixels and glyphs are rasterized
by swash on the CPU on every OS, so golden runs pin the fonts
(`COPILOT_CANVAS_FONTS=bundled`: Cascadia Mono plus an OFL UI font) and keep
one golden set per OS and scale: `tests/golden/gui/macos-2x/`,
`tests/golden/gui/linux-1x/`. The Linux set is captured in the CI container
with llvmpipe, which is deterministic, so it can **block** there (Windows's
goldens are advisory on CI because they belong to the reference machine).
The macOS set belongs to a reference Mac and is advisory on CI, as on
Windows.

### 7.2 CI matrix

| Job | Runner | Steps |
|---|---|---|
| `windows` (existing `ghostty` + `test`) | `windows-latest` | unchanged |
| `ghostty-unix` | `macos-15`, `ubuntu-24.04` | Zig 0.17.0 (pinned checksum), `cargo xtask build-ghostty --target …` for `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu` (and `aarch64` on `ubuntu-24.04-arm`); cached by the checksum file **and** the target triple |
| `macos` | `macos-15` (arm64) | clippy `-D warnings` and `cargo test --workspace` with Ghostty; `--no-default-features` build; `cargo xwin clippy` for Windows; release build; `corpus check`; TUI `selftest` (+ remote loopback, faults, paired); GUI `selftest --client gui`, `--parity`, `--hardening` on Metal (GitHub's Apple-silicon runners expose a paravirtual Metal device, which wgpu's own CI uses: confirm in Phase 0); goldens advisory |
| `linux` | `ubuntu-24.04` | the same tests; GUI runs twice: under **Xvfb** (X11) and under a headless Wayland compositor (`weston --backend=headless` or `sway` with `WLR_BACKENDS=headless`), with `mesa-vulkan-drivers` (lavapipe) and `dbus-run-session` (notifications, AT-SPI, Secret Service via `gnome-keyring --unlock` or the file backend); goldens blocking |
| `linux-oldest` | `ubuntu-22.04` container | release build for glibc 2.35, smoke test of the packages |
| `package` (tags) | `macos-15`, `ubuntu-22.04` | §8; signing and notarization only with the repository's secrets |
| `ios` (existing) | `macos-15` | unchanged |

Caching: `Swatinem/rust-cache` per OS and target; Linux build packages:
`libxkbcommon-dev libwayland-dev libx11-dev libxcursor-dev libxi-dev
libxrandr-dev libvulkan-dev mesa-vulkan-drivers xvfb weston dbus-x11
at-spi2-core` plus the WebKit development package chosen in D9.

## 8. Packaging and distribution

### 8.1 macOS

- **Bundle** (`cargo xtask bundle-macos`, new): `Copilot
  Canvas.app/Contents/MacOS/copilot-canvas-gui` (the bundle executable) and
  `…/MacOS/copilot-canvas` (service, CLI, TUI; the GUI already looks for its
  sibling), `Resources/AppIcon.icns` (from `assets/icon/make_icon.py`, which
  today writes `.ico`), bundled fonts, `Info.plist` (`CFBundleIdentifier`,
  `LSMinimumSystemVersion` 13.0, `NSHighResolutionCapable`,
  `LSApplicationCategoryType` developer tools, the folder usage descriptions
  of §5.3).
- **Universal binary**: build `aarch64-apple-darwin` and
  `x86_64-apple-darwin` (each with its own Ghostty archive) and `lipo` them;
  smoke-test the Intel slice under Rosetta on the arm64 runner.
- **Signing**: Developer ID Application, hardened runtime
  (`--options runtime`), timestamp; inner executables first, then the
  bundle. No entitlements are needed: no sandbox, no JIT; Apple Events only if
  the AppleScript route to Terminal were used (the `.command` file route
  avoids it).
- **Notarization**: `xcrun notarytool submit --wait`, `xcrun stapler
  staple`, for the app and the DMG. The owner's paid Apple developer account
  (IOS-PLAN) covers it; CI needs the certificate and an App Store Connect API
  key as secrets.
- **Channels**: a DMG on GitHub Releases and a Homebrew cask (`app` plus
  `binary` for `copilot-canvas`), first in the owner's tap. Development
  builds keep working unbundled (`cargo run`), without toasts.
- **Runtime details**: the per-build service copy keeps its signature and
  loses the quarantine attribute; the `cli` copy for sessions likewise. No
  auto-update (none on Windows); Sparkle is an option later.

### 8.2 Linux

| Format | Use | Notes |
|---|---|---|
| tarball | everything | both binaries, `.desktop`, icons, metainfo, an install script into `~/.local` |
| AppImage | desktop, no root | GUI entry point; `AppRun cli …` for the CLI; the service runs from the per-build copy in `<home>/bin`, never from the AppImage's mount |
| `.deb`, `.rpm` | Ubuntu/Debian, Fedora/RHEL | `cargo-deb`, `cargo-generate-rpm`; depends on `libxkbcommon`, `libvulkan1` or Mesa GL, the WebKit package of D9; recommends `mesa-vulkan-drivers`, a Secret Service provider, `xdg-utils`, `fonts-noto-color-emoji` |
| Flatpak, Snap | not in the first release | a sandboxed terminal host must escape to the host for every PTY (`flatpak-spawn --host`), which defeats the sandbox and needs a host agent; Snap would need classic confinement. A client-only (remote) Flatpak could come later |

Builds: x86_64 and aarch64, glibc 2.35 baseline (Ubuntu 22.04 container),
Ghostty linked statically, rustls instead of OpenSSL (§5.9), `.desktop`
file with `StartupWMClass` and the Wayland `app_id` matching
`copilot-canvas` (needed by `LauncherEntry` and docks).

## 9. Phases and estimates

Assumptions: one engineer who knows this codebase (Rust, the protocol, the
parity suite), with a Mac, a Linux desktop (GNOME and KDE, X11 and Wayland,
an NVIDIA and an Intel/AMD GPU at some point) and access to the Windows CI;
Windows behaviour must stay unchanged; estimates include tests and docs.
"Unassisted" is conventional engineering. "AI-assisted" assumes the way this
repository was built (an agent writing most code and tests from precise
instructions, the engineer reviewing and driving): code-heavy phases are
taken at 0.4 of the unassisted time, OS-integration phases at 0.45–0.55,
packaging at 0.7 and dogfooding at 0.9, because signing, desktop variety,
manual IME checks and time-gated weeks compress little. Every phase ends
green as in PLAN.md (clippy, tests, the self-tests it touches, Windows CI).

| Phase | Scope | Exit criteria | macOS only | Linux only | Both | Assisted (mac / lin / both) |
|---|---|---|---|---|---|---|
| **0 Groundwork** | target-gate the Windows dependencies; `canvas-sys` with Unix `process`, `fs`, `spawn`, `host`, `ipc`, `net`, `secret` (file backend first); `PathStyle` and `BackendInfo::platform`; Ghostty for Unix targets (xtask, build script, per-target ABI manifests); CI jobs for the portable and `canvas-sys` tests | `cargo check --workspace` (with and without Ghostty) and `cargo test` of the touched crates green on the OS; Windows CI and `cargo xwin clippy` green | 3.5 | 3.5 | 4.5 | 1.4 / 1.4 / 1.8 |
| **1 Service and TUI** (M0, M1) | **M0** first (≈ 2 weeks): `canvas-core` connection, tunnel child, Keychain secrets, `remote add/servers/forget` → the TUI on a Mac drives the Windows Cloud PC. Then `canvas-host` server on UDS, PTY kill path, orphan handling, priority v1, limits; `canvas-agents` on Unix (discovery, environment resolution, argv detection, paths, OpenCode listener check, service clipboard, open/reveal, `cli` install); CLI and `doctor`; demo shells; stand-ins | M0: REMOTE.md checklist 1–12 from a Mac TUI. **M1**: `cargo test --workspace`, `corpus check`, `selftest`, `selftest --remote loopback\|faults\|paired`, backend contract, `selftest agent --paste` (OpenCode, Claude Code), `selftest agent --agent opencode --prompt`, `bench --terminals 100` within PLAN §9 | 5 | 5 | 6.5 | 2 / 2 / 2.6 |
| **2 Lifecycle and automations** | `service install`; `Scheduler` trait; Launchd / SystemdUser / Cron backends; the built-in launcher; import, apply preview, backups, drift, run, enable; tunnel manifest | the TUI self-test's automation steps and the factory probes on the backend; a real tunnel hosted by the manifest on a Mac or Linux machine; `doctor` clean | 2.5 | 3 | 4.5 | 1.1 / 1.4 / 2 |
| **3 GUI core** (M2) | Metal / Vulkan / GL; `NativeWindow`; fonts and fallbacks; `KeyPolicy` in the core and the GUI (D8); IME; DPI; clipboard text and images; links; drag and drop (Wayland included); several windows; placement; counters; crash log; device loss; test hooks over UDS | `selftest --client gui` (39 steps) local and `--remote loopback`; `bench --client gui` meets PLAN §9 (keystroke → present p50 ≤ 8 ms, p99 ≤ 20 ms; idle ≤ 1 %; memory ≤ 250 MB) | 4 | 4.5 | 6 | 1.8 / 2 / 2.7 |
| **4 GUI platform integration** | HTML view (D9; Linux spike first); notifications; Dock / LauncherEntry; attention; title; menus and quit semantics; reduced motion; opening the other client; accessibility | parity rows 17, 41, 41b, G2 pass; manual IME (Japanese, Chinese, Korean), dead-key, AltGr and VoiceOver/Orca checks recorded | 3 | 4.5 | 7 | 1.7 / 2.5 / 3.9 |
| **5 Parity, goldens, hardening** | the 60 parity rows, the 9 hardening checks (with §7 substitutions), goldens per OS, the GUI benches; CI GUI jobs | `selftest --client gui --parity`: PASS 60 rows on the OS; `--hardening` PASS; goldens recorded and reviewed | 2 | 2.5 | 3.5 | 0.8 / 1 / 1.4 |
| **6 Packaging** | §8 | install from the artifacts on clean machines (macOS 13 and current; Ubuntu 22.04/24.04, Fedora, a KDE desktop); notarization accepted; first-run flows (permissions, keyring) | 1.5 | 2 | 3 | 1.1 / 1.4 / 2.1 |
| **7 Dogfood and rollout** | a dogfood week per OS with the TUI one key away; a 48 h remote soak with the OS as client and as service; README and docs per platform | no open parity bug for a week; soak budgets of REMOTE.md met | 1.5 | 2 | 3 | 1.4 / 1.8 / 2.7 |
| **Total** | | | **23** | **27** | **38** | **≈ 11 / ≈ 13½ / ≈ 19** |

Ranges: ± 15–20 % (macOS 19–27, Linux 22–31, both 32–44 unassisted). The
largest uncertainties are the Linux HTML view (± 2 weeks) and how many
agent-CLI differences appear on Unix.

**Milestones.**

| Milestone | Gives | After (unassisted / assisted) |
|---|---|---|
| **M0** macOS TUI client of the Windows service | the owner's Mac at home on the Cloud PC over the Dev Tunnel | ≈ 2 weeks / ≈ 1 |
| **M1** macOS app: local service + TUI | agents running on the Mac; every TUI feature, automations still pending | ≈ 8½ weeks / ≈ 3½–4 |
| **M2** macOS GUI | the GPU canvas locally and remotely (without the HTML view and OS polish) | ≈ 12½ weeks / ≈ 5 |
| **M3** macOS parity and package | notarized app, 60 parity rows | 23 / ≈ 11 |
| **L1–L3** Linux service + TUI, GUI, parity and packages | the same on Linux | +15 / +8 after M3, or 27 / 13½ alone |

**Critical path:** Phase 0 (`canvas-sys` process + IPC) → Phase 1 (service on
Unix) → Phase 3 (GUI core) → Phase 4 (HTML view, longest on Linux) →
Phase 5 → Phase 7. **In parallel:** the Ghostty build and the CI jobs (Phase
0, independent); M0 (needs only the client pieces of Phase 0); Phase 2
(automations) alongside Phase 3; the Linux HTML spike from the first week;
packaging (Phase 6) once Phase 3 runs; secrets backends beyond the file
store. With **two engineers** (shared Phases 0–1 together, then one per OS)
both platforms take about 25 calendar weeks unassisted, about 12 assisted.

## 10. Risks

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| **Linux HTML view**: no Wayland embedding; WPE packaging and bindings; input forwarding | high | high (row 41b) | a 1-week spike first (§5.7); formatted-text fallback ships meanwhile (Windows already has it); X11 child via XWayland as plan B; Blitz as plan C |
| **Secret storage UX**: Keychain prompts across GUI, TUI and service binaries and per-build copies; Linux without a keyring | medium | medium | one master-key item with both executables in its access list, same Developer ID; the 0600 file backend with a `doctor` warning; `COPILOT_CANVAS_SECRETS=file` |
| **Process control parity**: no raised priorities, no job objects, no subreaper on macOS | medium | medium (typing latency under load, orphans) | §5.2/§5.3 designs; `bench --burn 40` and an orphan test (MCP servers that `setsid`) as Phase 1 exit checks |
| **Environment resolution**: agents, `node`, `gh` missing from launchd/systemd `PATH`; no `LANG` | high | high (agents "not installed") | login-shell environment capture; `doctor` shows the resolved `PATH`; *Reload environment*; UTF-8 locale default |
| **macOS keyboard model**: Cmd/Ctrl/Option conflicts, Ctrl-click, F-keys behind Fn, agents' own keys (image paste) | medium | medium | D8 as one `KeyPolicy`; key-table tests per OS; the owner's dogfood week; a setting for Option as Meta |
| **Agent CLIs differ on Unix**: Copilot is a Node script (process names), Claude's `procStart`, data paths, image paste keys | medium | medium | `selftest agent` for each agent on each OS in Phase 1; argv matching; fallbacks already in the code (`startedAt`) |
| **Ghostty on macOS/Linux**: Zig 0.17 build of the pin, C++ runtime linking, LP64 ABI of the hand-written bindings | medium | medium | per-target ABI manifest tests; alacritty fallback per build and at run time (exists) |
| **TCC prompts** attributed to Terminal or to the app depending on who started the service | medium | low–medium | start through launchd from the bundle (§5.3); usage descriptions in `Info.plist` |
| **Linux desktop variety**: GNOME without progress or global menus, winit 0.30 without Wayland drag and drop, NVIDIA Vulkan, fractional scaling, IME frameworks | high | medium | test matrix (GNOME/KDE × X11/Wayland); own `wl_data_device` code or winit 0.31; GL fallback |
| **CI GPU and goldens**: Metal on hosted macOS runners, lavapipe determinism, font versions | medium | low | pinned bundled fonts; Linux goldens in a fixed container; macOS goldens advisory; confirm Metal on runners in Phase 0 |
| **Mixed-OS remote**: Windows paths on a Unix client and back; case sensitivity | medium | medium | `PathStyle` from the service (D10); contract tests with a Windows service and a Unix client in CI (loopback with a forced style) |
| **Packaging**: notarization of a bundle with a second executable, quarantine on copies, AppImage mounts | low | medium | sign inner executables first; strip quarantine on copies; per-build copy outside the mount |
| **Limits on macOS**: 256 open files for launchd jobs, 511 PTYs, per-user processes | medium | medium (many sessions) | raise `RLIMIT_NOFILE`; `doctor` reports PTY and process limits; idle auto-stop |
| **Parity drift**: Windows features keep landing (the owner's TODO bot) while the port is under way | high | medium | every new feature updates the platform layer for all three; the parity suite runs on all three in CI from Phase 5; new OS calls only through `canvas-sys` |

## 11. Open questions (owner decisions)

**Decided 2026-10-30:** every recommended option below is approved. Where no
option was marked as recommended, the default is: 3, the spike, with WPE
offscreen as the default; 6, glibc 2.35 with x86_64 and arm64; 8, the
proposed channels; 10, the same as Windows (ask whether to detach or stop
all), for 1:1 behaviour; 13, `PRIO_DARWIN_BG` on, with the same setting to
turn it off as on Windows; 14, yes; 17, yes once the owner provides the
certificate and the key (until then CI builds unsigned and notarization is
done by hand).

1. **GUI stack:** keep winit + wgpu + glyphon everywhere (recommended), or
   native AppKit / GTK front ends?
2. **Keys on macOS:** Cmd as the app modifier with every Ctrl combination
   passed to the terminal (recommended, D8), or the Windows Ctrl map on the
   Mac too? Left Option as Meta by default?
3. **Linux HTML view:** approve the spike and the default (WPE offscreen),
   or accept X11-only embedding (GUI on XWayland) or Blitz?
4. **Wayland-first on Linux** (recommended) or X11/XWayland-first?
5. **Minimum macOS** 13 (recommended) or 14+; **universal** binary
   (recommended) or Apple silicon only?
6. **Linux baseline:** glibc 2.35 (Ubuntu 22.04) and arm64 in the first
   release?
7. **Flatpak/Snap:** none in the first release (recommended)?
8. **Distribution channels:** Homebrew cask in an own tap first; DMG;
   AppImage + `.deb` + `.rpm` + tarball?
9. **Data home:** keep `~/.copilot-canvas` on every OS (recommended) or use
   `~/Library/Application Support` / XDG folders?
10. **Closing the last window on macOS:** detach silently and stay in the
    Dock (Mac convention) or ask detach / stop all as on Windows?
11. **Secrets:** Keychain / Secret Service with a master key (recommended), or
    0600 files only (simpler, weaker)?
12. **Service start:** on demand by clients as on Windows plus an optional
    `service install` (recommended), or always a launchd/systemd service?
13. **Priority manager on macOS:** is `PRIO_DARWIN_BG` for background
    sessions (slower disk, efficiency cores) acceptable, or off by default?
14. **Image paste without a service clipboard:** accept "upload and paste the
    path" on headless Linux?
15. **Order:** macOS first with M0 (recommended), Linux right after; or both
    in parallel with two engineers?
16. **Automations launcher:** may the built-in Rust launcher replace the
    Python one on macOS/Linux (recommended), with `launcher`/`python` still
    honoured?
17. **Apple signing in CI:** may the Developer ID certificate and an App
    Store Connect key be added as repository secrets?

## 12. What could not be determined here

- **Ghostty's static library for macOS and Linux** was not built: Zig is not
  installed on this Mac and installing system packages was out of scope.
  Whether the pinned commit builds `-Demit-lib-vt` for those targets with
  Zig 0.17.0, and whether the archive needs `libc++`/`libstdc++` at link time,
  is open (Phase 0).
- **Linux checks are type-checks only**: a stub C compiler stood in for the
  missing cross-compiler, so nothing was linked; real Linux builds run first
  on a Linux runner (Phase 0).
- **The crates that do not build** were checked only to the first layer
  (name resolution); the scratch experiment shows the next layer for
  `canvas-agents`, `canvas-core` and `canvas-host`, not for `canvas-gui`.
- **Agent CLIs on Unix:** the Copilot CLI's process names, data folder under
  `XDG_CONFIG_HOME`, and image paste key; Claude Code's `procStart` format;
  which agents read images from the clipboard on macOS and Linux. None of the
  agents was run here.
- **Metal on GitHub's macOS runners** for the GUI tests (believed available
  through the paravirtual device; to be confirmed).
- **swash and emoji fonts:** sbix and CBDT are in swash's source, COLRv1 is
  not; how Apple Color Emoji and Noto Color Emoji look through glyphon was
  not rendered here.
- **WPE WebKit** availability and API stability on the target distributions,
  and Blitz's fidelity on real agent reports (the spike).
- **Keychain access lists** for a second executable copied out of the
  bundle (prompt behaviour needs a signed build to test).
- **Effort calibration** for AI assistance: the factors in §9 are judgement,
  informed by this repository's history (PLAN.md's 12–17 week plan landed
  in days), not measured.

## Appendix: how the checks were run

```sh
source ~/.cargo/env
cargo check -p canvas-common --all-targets          # and each crate, one at a time
cargo check -p canvas-host --no-default-features --all-targets
cargo check -p canvas-host --features ghostty       # build script refuses non-MSVC targets
cargo check -p copilot-canvas [--no-default-features]
rustup target add x86_64-unknown-linux-gnu
CC_x86_64_unknown_linux_gnu=<stub cc> AR_x86_64_unknown_linux_gnu=<stub ar> \
  X86_64_UNKNOWN_LINUX_GNU_OPENSSL_LIB_DIR=<placeholder> \
  X86_64_UNKNOWN_LINUX_GNU_OPENSSL_INCLUDE_DIR=<placeholder> \
  cargo check -p <crate> --all-targets --target x86_64-unknown-linux-gnu
```

Logs, the stub compiler and the scratch copy (Windows dependencies
target-gated; `canvas-host`'s `term`/`server` un-gated) live in the session's
temporary folder, not in the repository.
