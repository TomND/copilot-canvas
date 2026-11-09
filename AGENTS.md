# Atelier: instructions for agents

You are working on **Atelier**, a native Windows app that shows agent
CLI sessions (GitHub Copilot CLI, OpenCode, Claude Code) as boxes on an
infinite canvas, with real terminals, a notebook, todo boards and passive
agents. Rust workspace, edition 2024, Windows only for the desktop
(an iPhone client and a macOS mock exist). This file is the short version of
how to work here; the in-depth guide is [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).

<!-- rename:keep-begin -->
Atelier was called Copilot Canvas. A branch made before the rename runs
`python3 tools/rename-to-atelier.py` before merging (docs/DEVELOPMENT.md §7);
old names live only in `canvas_common::legacy` and on `rename:keep` lines;
read variables with `canvas_common::env::var`.
<!-- rename:keep-end -->

## Read first

1. This file.
2. [README.md](README.md): what every feature does (the *Features* reference,
   *Controls*, *Commands*, *Architecture*, *Safety*). The README is the
   product's specification: keep it true.
3. [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md): code map by task, recipes,
   the test layers, pitfalls.
4. The topic doc of what you change (table at the end).
5. `PLAN.md` §0 for the project's status; `git log` for recent conventions.

## The shape of the system

```
clients (TUI, GPU GUI, iPhone) ──pipe/TCP──▶ atelier service (one per user)
  canvas-core: app state, layout              terminals (ConPTY + libghostty-vt)
  in character cells, shared                  the canvas document (single writer)
  by the TUI and the GUI                      event bus, passive agents, stores
                                              (reports, PRs, reviews, todos),
                                              the backend RPC (files, git, agents)
```

| Crate | Role |
|---|---|
| `canvas-common` | wire protocol (postcard), pipe/TCP transports, paths, settings, Windows helpers |
| `canvas-host` | L1 terminal host: ConPTY, emulators (ghostty, alacritty), frames, input |
| `canvas-termview` | L2 client-side terminal model, selection, links |
| `canvas-model` | the document (`doc.rs`), patches (`sync.rs`), bus, RPC types (`rpc.rs`), todos, notebook, skills, reports, PRs, reviews: **pure and portable** |
| `canvas-agents` | L3 backend and the service: agent providers, history index, bus loop, passive runs, stores, the agent-facing CLI pieces |
| `canvas-core` | the app state machine and all UI layout in cells (canvas, sidebar, dialogs, spaces, notebook, tasks, editor) |
| `canvas-tui` / `canvas-gui` | front ends: the TUI paints the cells in a console; the GUI paints the same cells with wgpu/glyphon plus a GPU canvas model and a WebView2 pane |
| `canvas-mobile`, `canvas-mock`, `canvas-push` | iPhone core (UniFFI), the macOS stand-in service, APNs |
| `canvas-corpus`, `ghostty-vt-sys`, `xtask` | golden terminal corpus, Ghostty FFI, `cargo xtask build-ghostty` |
| `src/` | the `atelier` binary: CLI dispatch (`main.rs`), `selftest`, `parity*`, `golden`, `hardening`, benches |

## Build

- Stable Rust MSVC, Visual C++ build tools, Windows SDK. The default terminal
  engine needs Zig 0.17.0: `cargo xtask build-ghostty` once per pinned
  version (docs/GHOSTTY.md). In a new git worktree, copy `target\ghostty`
  from the main checkout instead of rebuilding it.
- `cargo build --release` builds `atelier.exe` and
  `atelier-gui.exe`; `--no-default-features` uses alacritty (no Zig).
- A cold build takes about 10 minutes; seeding a new worktree's
  `target\debug` from another checkout saves most of it.

## Verify before you commit

Run in this order and fix every failure (details and timings: the
`canvas-verify` skill, docs/DEVELOPMENT.md):

```text
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release
target\release\atelier.exe selftest                      (also --remote loopback, --remote faults, --remote paired)
target\release\atelier.exe selftest --client gui         (also --client gui --remote loopback)
target\release\atelier.exe selftest --client gui --parity
target\release\atelier.exe selftest --client gui --golden
target\release\atelier.exe selftest --client gui --hardening
target\release\atelier.exe corpus check
```

- Documentation-only changes need none of it; code changes need at least
  clippy and the tests; anything a user can see needs the self-tests and
  the parity suite too.
- GUI suites open real windows on the owner's desktop: run them **one at a
  time**, never two together. They use sandboxes (their own instance and home).
- `selftest agent` drives the real agent CLIs: only when asked.
- The portable crates are also built on macOS in CI: `cargo clippy -p canvas-common -p canvas-model -p canvas-termview -p canvas-push -p canvas-mobile -p canvas-mock --all-targets -- -D warnings` and `cargo clippy -p canvas-host --no-default-features --all-targets -- -D warnings` (with their tests).

## Rules that keep the product right

- **Both clients.** Every feature works in the TUI and the GUI: UI state and
  layout live in `canvas-core` (cells); the GUI adds pixel polish only. HTML
  is GUI-only with a formatted-text fallback in the TUI. Record GUI-only
  behaviour in docs/PARITY.md.
- **Remote mode.** Clients never touch the service machine's files or
  processes directly: everything goes through the backend RPC
  (`canvas_model::rpc::Request`, handled in `canvas-agents/src/backend.rs`) or
  the service. Paths are the service machine's.
- **Viewing never starts anything.** A click, Tab, arrows, a badge or a link
  only views; only a double-click, Enter, an explicit Start/Resume or a menu
  item starts or resumes a session (WI-007).
- **One writer.** The service owns `canvas.json` and every store (reports,
  PRs, reviews, todos); clients send edits or ops and apply what comes back.
  The service never originates document patches.
- **Compatibility** (older services and clients stay in use):
  - new JSON fields: `#[serde(default, skip_serializing_if = …)]`;
  - wire enums (`ClientMsg`, `ServiceMsg`, `Request`, `Response`): append
    variants at the **end** only, and even then an older peer cannot decode
    them: a new `ServiceMsg` goes only to clients with a capability bit
    (`CAP_*`, `ServiceMsg::shaped_for`), a new `ClientMsg` only to services
    that announce support (an older service drops the connection), a new
    `Request` gets "does not support" from an older backend; postcard
    messages such as `AgentLaunch` cannot gain fields (carry extras in a JSON
    part or `env`);
  - never add a `NodeKind` (no fallback variant: older peers fail to parse);
  - a new `SetField` name is refused by older services (the edit resyncs).
- **Portable crates** (`canvas-model`, `canvas-common`, `canvas-termview`,
  `canvas-mobile`, `canvas-mock`, `canvas-push`): no Windows-only APIs.
- **Glyphs** in cells must be one cell wide in the Windows console too
  (`☰` is not: use `≡`); no emojis in code or UI text.
- **Safety:** agent HTML is sanitized and shown in a locked-down WebView2;
  never send prompts to paid models from tests; never post comments, votes,
  merges or messages to real services in tests; tests use temporary homes,
  never the real `~/.atelier` or the owner's repos.

## Working conventions

- No rustfmt: match the surrounding style (long lines are fine). Clippy with
  `-D warnings` must be clean. Concise doc comments in plain English.
- Tests with every change: unit tests next to the code, view snapshots
  (`crates/canvas-core/tests/view_snapshots.rs`; `UPDATE_SNAPSHOTS=1`, then
  read every `.snap` diff), backend contract tests for new requests, a
  self-test step and/or a GUI parity row for user-visible behaviour, golden
  screenshots re-blessed only after looking at them (`--golden --update`).
- Docs with every change: the README feature reference and controls, the
  topic doc, docs/PARITY.md (rows and the *Last run* section).
- Commits: one subject line that says what changed and why, often long, in
  the repository's style (`git log`); backlog items start with their id
  (`WI-023 …`). Do not push unless asked; never force-push.
- The owner's machine is in use: run builds at below-normal priority with a
  limited job count (`$env:CARGO_BUILD_JOBS`), write long outputs to a file
  and read its tail, never stop processes you did not start (the owner's
  canvas service and agent sessions run there).
- Agent tool runners abort very large single writes: create big files in
  parts.

## Where to look

| Topic | Doc |
|---|---|
| Code map, recipes, test layers, pitfalls | [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) |
| Service, protocol, RPC, stores, compatibility | [docs/BACKEND.md](docs/BACKEND.md) |
| GPU client, web pane, test hooks, measurements | [docs/GUI.md](docs/GUI.md) |
| TUI/GUI parity matrix and the suite | [docs/PARITY.md](docs/PARITY.md) |
| How the canvas runs Copilot, OpenCode, Claude Code (launch, status, skills) | [docs/AGENT-PROVIDERS.md](docs/AGENT-PROVIDERS.md) |
| Notebook, Tasks, spaces (design and as built) | [docs/NOTEBOOK-TASKS.md](docs/NOTEBOOK-TASKS.md) |
| Remote mode, pairing, Dev Tunnels | [docs/REMOTE.md](docs/REMOTE.md) |
| Terminal engine (libghostty-vt) | [docs/GHOSTTY.md](docs/GHOSTTY.md), [corpus/README.md](corpus/README.md) |
| iPhone app | [ios/README.md](ios/README.md), [docs/IOS-STATUS.md](docs/IOS-STATUS.md) |
| macOS and Linux plan | [docs/MAC-LINUX-PLAN.md](docs/MAC-LINUX-PLAN.md) |

## Skills in this repository

`.claude/skills/` (read by Claude Code, OpenCode and Copilot CLI):
`canvas-verify` (the verification above, run safely), `canvas-add-feature`
(the checklist for a change a user can see), `canvas-wire-compat` (changing
protocol, document and store types), `canvas-ui-testing` (snapshots, test
hooks, parity rows, goldens).
