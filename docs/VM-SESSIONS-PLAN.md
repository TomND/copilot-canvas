# Atelier VM sessions: implementation plan

Status: **approved by the owner on 2026-11-09**, with every decision of §1 as recommended (the ten
"top decisions" below in particular). Source design: [VM-SESSIONS.md](VM-SESSIONS.md). Progress,
the owner's checklist and the working log: [VM-STATUS.md](VM-STATUS.md). This file is the plan as
written before the implementation (file:line references are `main` @ 220a5f5, before the rename to
Atelier); where the implementation departs from it, VM-STATUS.md says so.

**VM sessions are optional; local stays the default (owner, 2026-11-09).** Without a VM pack nothing
changes and no VM UI shows (no *Run on* field, no ☁ panel; `atelier vm pack check` only says that no
pack is configured). With a pack, *Run on* defaults to *This machine* and `Placement` defaults to
`Local` everywhere (box, canvas and group launch defaults, board policy). A VM is chosen only
explicitly: per session, as a group's or the canvas's launch default, or by a board's *Start on VM*.
Old documents and canvases behave exactly as before. Tests prove it: the view snapshots and the
self-test without a pack are unchanged, and a unit test checks the defaults.

Approved decisions (owner, 2026-11-09):

1. Nested `ClientMsg::Sat` / `ServiceMsg::Sat` at index 29 with their own satellite protocol;
   `CAP_SATELLITE = 1 << 6`.
2. Satellite credentials in `<home>/vm/satellites.json` with `sat-` ids; the role is decided in
   the handshake, before `Ready8`.
3. `agent: "vm:<kind>"` so older services refuse VM boxes, and the service checks the document's
   placement too.
4. The satellite is the Linux release build, a CI artifact named with its SHA-256 in the pack,
   uploaded into images and at claim.
5. Terminal queries are answered in the guest (alacritty engine).
6. Typing is refused while the link is down; buffered (4 KiB, 30 s) while a suspended VM resumes.
7. The platform's auto-suspend and auto-delete are off.
8. The token audience is a pack setting, defaulting to the SDK's.
9. v1: passive runs and canvas-wide agents stay on the hub; forking a VM session is refused.
10. ☁ VMs is a toolbar panel; the glyph is checked to be one cell wide (fallback `⌂`).

Contents
- §0 Baseline and code facts this plan rests on
- §1 Decisions this plan takes (summary; reasoning in §5)
- §2 Wire and storage additions, with exact placement
- §3 Phases F (foundations), M, 0, 1, 2, 3, 4, 5: work items, files, types, tests, exit criteria
- §4 The CLI contract inventory
- §5 Where the design is underspecified or conflicts with the code, and the resolution
- §6 Order of work and parallelism
- §7 Risks and estimates
- §8 The owner's checklist (spikes S1–S7, `selftest vm --real`) and the example pack
- Appendix A: Azure Container Apps Sandboxes data-plane REST, as the SDK 0.1.0b4 calls it
- Appendix B: what `vm::local` proves in CI on Windows, macOS and Linux

---

## 0. Baseline and code facts this plan rests on

**Branch state.** `main` @ 220a5f5 already holds the macOS and Linux ports (`origin/linux` and
`origin/macos` are merged: `git log main..linux` is empty). So the same `atelier` binary builds for
`x86_64/aarch64-unknown-linux-gnu` (glibc 2.35 floor, CI job `linux` on ubuntu-22.04, 24.04 and
24.04-arm) and `crates/canvas-sys` (process, ipc over Unix sockets, fs, secret, spawn, net, host)
exists. This removes the design's biggest unknown for the guest side: the satellite is the existing
Linux build plus new code.

**Wire indices today** (`crates/canvas-common/src/protocol.rs`): the last `ClientMsg` is
`PushRegister` at index 28, the last `ServiceMsg` is `TunnelAccess` at index 28; the last
capability bit is `CAP_TUNNEL_ACCESS = 1 << 5`. `origin/ios-client`, `origin/linux`,
`origin/macos` agree. Re-check after the rename lands (it must not add variants, but verify).

**Facts that shape the design (each was checked in the code):**

| # | Fact | Where | Consequence |
|---|---|---|---|
| C1 | `Host::launch` calls `HostApp::resolve_launch` synchronously on the client's reader thread, then `Terminal::spawn` | `canvas-host/src/server.rs:618-657`, `dispatch` 1110 | a VM launch must create its terminal at once and provision in the background (design: "`resolve_agent` records the intent and returns at once") |
| C2 | `Terminal` owns `master: Mutex<Option<Box<dyn MasterPty>>>`, `killer: Mutex<Box<dyn ChildKiller>>`, an input channel drained by a writer thread, an output channel fed by a reader thread, and a waiter thread | `canvas-host/src/term.rs:77-336` | a `Remote` PTY replaces the three threads and the master; `force_kill` goes through `killer` (a remote `ChildKiller` adapter keeps `stop()` unchanged) |
| C3 | Every process-tree loop filters `t.pid != 0` (priority, remember, subsessions, OpenCode pid, requester) | `server.rs:1449,1497`, `service/mod.rs:581,790,955` | remote terminals carry `pid: 0` and are skipped for free |
| C4 | The emulator answers queries through `Sink::reply` → the input channel | `term.rs:198-201` | for remote terminals the replies must be suppressed on the hub (the guest answers), see §5.6 |
| C5 | `serve_client` sends `Ready8`, the terminal list and `client_connected` (backend snapshots) right after `handshake::serve`, and inserts the client in `host.clients` (used by `broadcast`, `visible`, liveness, clipboard) | `server.rs:874-1016` | the satellite role must branch off **before** `Ready8` and satellites must never enter `host.clients` |
| C6 | The handshake asks `HostApp::remote_credential(id)` for a paired id; `Credential` is an in-process enum (`Secret`, `Revoked`, `Refused`) | `canvas-common/src/handshake.rs:84-200` | a `Credential::Satellite(secret)` variant and `Accepted::satellite` carry the role out of the handshake without any wire change |
| C7 | `Host::shutdown` stops every terminal gracefully; `idle_loop` exits with no alive terminal and no client | `server.rs:675-698, 1617` | remote terminals must be detached, not stopped, on shutdown (A2b), and re-adopted on start |
| C8 | Local clients answer backend requests in-process (`LocalBackend`), which has only `Ctx { home, sessions_dir }`; requests that need service state are answered by the service itself for pipe clients only (`is_remote_admin`) | `canvas-agents/src/backend.rs:26-57`, `service/mod.rs:554-567` | VM state cannot be an ordinary backend request for the local GUI/TUI: the UI reads a bus snapshot view and acts with bus actions; history resolution for `Transcript` must be file-backed (§5.12) |
| C9 | Providers build commands from hub state: `self.program()` (hub PATH), `group_dirs()` creates hub folders, `COPILOT_CUSTOM_INSTRUCTIONS_DIRS` is read from the hub's environment, Claude Code writes a private MCP config file on the hub, paths are `PathBuf::display()` (backslashes on a Windows hub) | `agents/copilot.rs:47-119`, `agents/claude.rs`, `agents/mod.rs:133` | `ExecTarget` needs target-aware command building that never touches hub files or hub env for a VM target and builds guest paths with `PathStyle::Posix` |
| C10 | Copilot history index is per root (`Index::new(root)`), liveness from `inuse.<pid>.lock` checked against local pids; Claude Code reads `~/.claude/projects` (JSONL); OpenCode reads a SQLite database plus its TUI's HTTP server | `index.rs:381-500`, `agents/copilot.rs:121-135` | JSONL agents can be mirrored by appends; OpenCode cannot (§5.10); liveness must be overlaid from satellite reports |
| C11 | Passive digests read `instance.sessions_dir.join(id).join("events.jsonl")` directly | `service/passive.rs:147,206` | a history consumer the design's list misses; it must go through `SessionStore::resolve` |
| C12 | The agent CLIs (`emit`, `send`, `status`, `followup`, `effect`, `report`, `pr`, `review`, `todo`, `note`) read `<home>/bus/routing.json` and stores, write inbox files or spool files and poll answer files | `cli.rs`, `prs_cli.rs:67-91`, `todos_cli.rs`, `notes_cli.rs`, `reports.rs:560-640`, `reviews_cli.rs` | §4 inventories them; satellite mode reuses the same functions on the hub through an `Io` seam |
| C13 | `AgentKind::parse` accepts only the three agent names | `canvas-model/src/agent.rs:62-69` | `agent: "vm:copilot"` makes an older service refuse a VM launch instead of running it locally (§5.13) |
| C14 | `LaunchDefaults` deserializes through `RawLaunchDefaults` | `canvas-model/src/doc.rs:280-320` | a new `placement` field must be added to both |
| C15 | The activity bar has 9 views bound to Alt+1…9 | `canvas-core/src/side.rs:36-46` | a ☁ VMs sidebar view would be a 10th; plan a toolbar panel instead (§5.24) |
| C16 | The client stops idle agent sessions itself (`idle_stop_minutes`) | `canvas-core/src/app.rs:2016` | VM boxes must be excluded; the service suspends them (`suspend_after`) |
| C17 | `canvas_common::ws` has both sides (`accept`, `connect`); there is no Rust TLS WebSocket client (the phone uses URLSession); `rustls` (ring) is already in the tree through `canvas-push` on Linux | `ws.rs:104-148`, `canvas-push/Cargo.toml` | the satellite needs a small `wss://` dialer (rustls + platform roots + HTTP proxy CONNECT) |
| C18 | Azure SDK `azure-containerapps-sandbox` 0.1.0b4 uses the scope `https://dynamicsessions.io/.default`; the design says audience `https://management.azuredevcompute.io` | SDK `_helpers.py:45` | the audience is a pack setting (§5.8) |
| C19 | The platform auto-suspends a sandbox that has no ingress, no exec, no interactive shell and no file operations (outbound traffic does not count), default on at 300 s in the SDK's create | Microsoft Learn overview; SDK `_sandbox_ops.py` create body | a working agent with only outbound traffic would be frozen: sandboxes are created with auto-suspend off (§5.9) |
| C20 | No Zig on this Mac and no `target/ghostty`; `cargo-xwin` is installed; ~12 GB free | this machine | local builds use `--no-default-features` (alacritty) with one shared `CARGO_TARGET_DIR`; Windows is checked with `cargo xwin clippy` and CI (§6.4) |

---

## 1. Decisions this plan takes (summary)

Each line is decided here unless marked **[owner]**; §5 gives the reasoning. The top ones are
repeated in the final message as recommendations.

| # | Decision |
|---|---|
| D1 | Satellite messages are **two nested enums**: `ClientMsg::Sat(Box<SatUp>)` and `ServiceMsg::Sat(Box<SatDown>)`, each appended at **index 29**, with their own append-only satellite protocol (`canvas_common::sat`, `SAT_PROTOCOL = 1`). The design's `Sat*` names become the inner variants. **[owner: confirm vs. one top-level variant per message]** |
| D2 | `CAP_SATELLITE = 1 << 6`, set by satellites; `ServiceMsg::shaped_for` drops `ServiceMsg::Sat` without it (defence in depth); it grants nothing |
| D3 | Satellite credentials live in **their own store** `<home>/vm/satellites.json` (ids `sat-` + 16 hex, sealed with `canvas_sys::secret` like `clients.json`), never in `clients.json`, so a downgraded service refuses them instead of treating them as paired clients. The role comes from the store in the handshake (`Credential::Satellite`), before `Ready8`; satellites are refused on the pipe. **[owner: the design says "a satellite role on stored credentials"; this is that role, in a separate file]** |
| D4 | A VM launch fails closed on older services: the launch's `agent` is `vm:<kind>` (refused by `AgentKind::parse` there), the placement travels in `AgentLaunch::env` as `ATELIER_PLACEMENT`, and the service also checks the document: a box placed on a VM is never launched locally (an older client's double-click is refused with "update this client") |
| D5 | The satellite is the same `atelier` Linux binary (glibc 2.35 build from CI's ubuntu-22.04 runners, published as a CI artifact `atelier-linux-<arch>`); the pack names the file and its SHA-256; it is uploaded into images when they are built and into a sandbox at claim when the satellite's reported protocol is outside the hub's range. `vm::local` runs the current executable. **[owner]** |
| D6 | The satellite's headless emulator is **alacritty** (pure Rust, no Zig, same palette answers as Ghostty: both use `emulator::alacritty::palette`) |
| D7 | The guest answers terminal queries (DA, CPR, OSC 10/11/4, kitty `CSI ? u`); the hub's emulator suppresses replies for remote terminals |
| D8 | Output sequence numbers are absolute byte offsets per `(terminal, generation)`; generations are hub-assigned, monotonic, persisted; ring 4 MiB per terminal (pack `link.ring_mb`, 1–16), 32 MiB per sandbox; gap reset = a VT repaint synthesized from the satellite's snapshot |
| D9 | Input while the link is down is refused with a notice (as in remote mode); input typed while a suspended VM resumes is buffered (≤ 4 KiB, ≤ 30 s) because the screen the user saw is exactly the frozen one |
| D10 | Agent CLIs in the guest (`ATELIER_REMOTE=satellite`): reads run locally on mirrored files (routing, todo book) and say when they are stale; writes are `CliOp`s relayed to the hub, executed by the hub's own CLI functions as the terminal the satellite proved (process tree), deduplicated by op id; notes are files in an `rw-notes` mirror up-synced with `SatUp::Note` |
| D11 | VM UI state reaches clients as `bus::Snapshot::vm: Option<VmView>`; windows act with window-only bus actions `{"action":"vm",…}`; the `Vm*` requests exist for the CLI, scripts and remote clients and are answered by the service itself (as `is_remote_admin` requests are) |
| D12 | Phase M's hooks become a permanent provider `vm::hooks` (claim, exec, suspend, resume, delete, image, shell-command), and the hook set gains `exec` (Phase M's `satellite --lite` needs it) |
| D13 | v1 scope: interactive sessions and their subsessions run on VMs; passive runs and canvas-wide agents stay on the hub; forking a VM session is refused (Phase 5 may add an in-guest fork) |
| D14 | Sandboxes are created with platform auto-suspend **off** and auto-delete **off**; the canvas suspends (`suspend_after`) and deletes (quiescence barrier) itself |
| D15 | Azure tokens come from `az account get-access-token --resource <audience>` (or a pack command), cached and refreshed in the background, never on the launch path; the audience is a pack field defaulting to the SDK's `https://dynamicsessions.io` **[owner: S-check]** |
| D16 | ☁ VMs is a toolbar panel (right pane, like ◎ Agents), not a 10th activity view; `☁` is used only after a one-cell check in conhost, Windows Terminal, macOS Terminal and the GUI grid, else `⌂` |
| D17 | The hub's history mirrors and the `SessionStore` index are files under `<home>/vm/` so the in-process `LocalBackend` resolves VM sessions for `Transcript`, `HasHistory` exactly as the service does |
| D18 | The MCP gateway terminates MCP Streamable HTTP in the satellite and relays JSON-RPC messages (not HTTP bytes) over `SatUp/SatDown::Mcp`; the hub runs the server per (session, server), filters and validates every message |
| D19 | Tests and CI use `vm::local` (satellite = local process in a temp home; suspend = freeze its process tree; egress = a local policy proxy stand-in; secrets = held by the stand-in, never in the guest home or environment) |
| D20 | Work starts after the rename lands on `main`; one branch `vm` in a worktree, merged phase by phase after the full verification of that phase |

---

## 2. Wire and storage additions, with exact placement

Rules followed (skill `canvas-wire-compat`): postcard enums only grow at the end and their structs
never gain fields; JSON gets `#[serde(default, skip_serializing_if = …)]`; no new `NodeKind`; new
`ServiceMsg` only to clients with a capability; a new `ClientMsg` only to services known to read
it; stores carry a format version and are never written over when newer or unreadable.

### 2.1 Protocol (`crates/canvas-common/src/protocol.rs`)

| Addition | Placement | An older peer |
|---|---|---|
| `pub const CAP_SATELLITE: u32 = 1 << 6;` | after `CAP_TUNNEL_ACCESS` (bit 5) | never sets it; a satellite connection is decided by its credential, not by this bit |
| `ClientMsg::Sat(Box<crate::sat::SatUp>)` | appended after `PushRegister`: **index 29** | only satellites send it, only to the hub that minted their credential (a current one). A downgraded hub refuses the credential before any `Sat` is sent (D3) |
| `ServiceMsg::Sat(Box<crate::sat::SatDown>)` | appended after `TunnelAccess`: **index 29** | written only by a satellite link's writer; `shaped(…)` returns `None` for protocol 7; `shaped_for` returns `None` without `CAP_SATELLITE` |
| `SERVICE_V7_VARIANTS` | unchanged (12) | — |

Tests: extend `protocol8_variants_round_trip_at_appended_indices` with `(29, ClientMsg::Sat(…))`
and `(29, ServiceMsg::Sat(…))`; extend `protocol7_clients_only_get_protocol7_shapes`; a new
`satellite_messages_only_reach_satellites` modelled on
`images_and_notifications_only_reach_clients_that_asked_for_them`.

### 2.2 The satellite protocol (`crates/canvas-common/src/sat.rs`, new, portable)

```rust
pub const SAT_PROTOCOL: u32 = 1;          // SatUp::Hello::version; the hub accepts [1, SAT_PROTOCOL]
pub const SAT_IO_CHUNK: usize = 16 << 10; // terminal input/output chunks (priority lane)
pub const SAT_FILE_CHUNK: usize = 1 << 20; // history, files, notes, MCP payload pieces

/// Satellite → hub, inside ClientMsg::Sat. Append-only (postcard index pinned by a test).
pub enum SatUp {
    Hello { version: u32, caps: u32, json: String },                                  // 0  design SatHello; json = HelloInfo
    InputAck { term: String, generation: u64, seq: u64 },                              // 1  design SatInputAck
    Output { term: String, generation: u64, seq: u64, bytes: Vec<u8> },                // 2  design SatOutput; seq = offset of bytes[0]
    Snapshot { term: String, generation: u64, seq: u64, cols: u16, rows: u16, vt: Vec<u8> }, // 3 gap reset at offset seq
    Exit { term: String, generation: u64, code: u32, json: String },                  // 4  design SatExit
    Proc { json: String },                                                             // 5  design SatProc: liveness, nested sessions
    History { root: String, file: String, ident: u64, offset: u64, bytes: Vec<u8> },  // 6  design SatHistory (+ ident)
    Op { id: String, term: String, json: String },                                     // 7  design SatOp (CliOp JSON)
    Note { id: String, root: String, name: String, base_rev: Option<String>, bytes: Vec<u8> }, // 8 design SatNote (+ id)
    Mcp { stream: u64, kind: u8, bytes: Vec<u8> },                                     // 9  design SatMcp
    Stats { json: String },                                                            // 10 design SatStats
    ControlResult { id: u64, json: String },                                           // 11 new: answers SatDown::Control
}

/// Hub → satellite, inside ServiceMsg::Sat. Append-only.
pub enum SatDown {
    Welcome { json: String },                                                          // 0  new: sandbox, root table, limits, attach list
    Spawn { term: String, generation: u64, argv: Vec<String>, env: Vec<(String, String)>, cwd: String, cols: u16, rows: u16 }, // 1
    Input { term: String, generation: u64, seq: u64, bytes: Vec<u8> },                 // 2
    Resize { term: String, generation: u64, cols: u16, rows: u16 },                    // 3
    Signal { term: String, generation: u64, signal: u8 },                              // 4  INT, TERM, HUP, KILL
    Stop { term: String, generation: u64, force: bool },                               // 5
    Resync { term: String, generation: u64, have: u64 },                               // 6  new: replay from `have` or Snapshot
    OpResult { id: String, json: String },                                             // 7  design SatOpResult
    File { root: String, name: String, rev: String, offset: u64, last: bool, bytes: Vec<u8> }, // 8 design SatFile (chunked)
    Delete { root: String, name: String },                                             // 9  design SatDelete
    Manifest { root: String, json: String },                                           // 10 design SatManifest
    Mcp { stream: u64, kind: u8, bytes: Vec<u8> },                                     // 11 design SatMcp
    Control { id: u64, json: String },                                                 // 12 new: git check, prepare branch, flush, upgrade, read file, shutdown
    NoteResult { id: String, json: String },                                           // 13 new: note stored (hub path, rev) or conflict
}
```

Everything that may grow later is a JSON part (`json`, `HelloInfo`, `WelcomeInfo`, `CliOp`,
`ControlOp`), because postcard variants cannot gain fields. Names (`root`, `file`, `name`) are
validated by both ends with `canvas_model::vm::sync_name_ok` (§2.5). `kind` of `Mcp`: 0 open
(JSON header: term, server, session id), 1 message (one JSON-RPC message), 2 close, 3 error.

Tests (`sat.rs`): every variant round-trips at its pinned index; frames of the largest chunk fit
`MAX_FRAME`; a `SatUp` with an unknown index is refused by the hub without closing other links.

### 2.3 Handshake (`crates/canvas-common/src/handshake.rs`, in-process types only)

- `Credential::Satellite(Vec<u8>)`: authenticate like `Secret`; refused when `remote == false`
  (never on the pipe).
- `Accepted { protocol, client, satellite: bool }` (in-process struct; no wire change).
- Revocation reuses `Credential::Revoked(key)`: a deleted sandbox's satellite gets a refusal it can
  verify (`pairing::revocation_key(secret)`), so it stops retrying (`Refused::revoked()`).

### 2.4 Backend requests (`crates/canvas-model/src/rpc.rs`), appended after `SkillSources`

| Request | Response (appended after `SkillSources`) | Lane |
|---|---|---|
| `VmList` | `Vm(Box<crate::vm::VmView>)` | quick (30 s) |
| `VmPool` | `Vm(…)` (the view's pool part filled) | quick |
| `VmImages` | `Vm(…)` | quick |
| `VmAction { r#box: String, op: crate::vm::VmOp }` | `Text` (what happened) | 120 s |
| `VmImageBuild { repo: String }` | `Text` (the build's id; progress in the view) | 120 s |
| `VmBudget` | `Vm(…)` | quick |
| `VmPackStatus` | `VmPack(Box<crate::vm::PackStatus>)` | 60 s |

`Request::is_service_state()` (new, like `is_remote_admin`): these are answered by `Agents` in
`service/mod.rs:message` (it owns `vm::Vm`); `LocalBackend` answers them with
`Response::Error("ask the service: …")`. Clients on the pipe and paired remote clients may ask;
satellites never reach this code (they have no `Client`). Tests: the round trip and renamed-variant
answer (pattern `skill_source_requests_round_trip_and_old_services_say_so`), and a
`backend_contract` test that the service answers them over loopback TCP while `LocalBackend`
refers to the service.

### 2.5 Model (`crates/canvas-model`)

| Where | Addition | Serde | An older peer |
|---|---|---|---|
| `doc.rs` `Node` | `placement: Option<Placement>` | `default, skip_serializing_if = Option::is_none` | ignores it; a double-click there sends a local launch, which the current service refuses for a VM box (D4) |
| `doc.rs` `LaunchDefaults` and `RawLaunchDefaults` | `placement: Option<Placement>` (canvas and group: *new sessions run on…*) | same | an older client rewrites `defaults` without it (as with `skill_dirs`): documented |
| `doc.rs` (new types) | `enum Placement { Local, Vm(VmPlacement) }` (`#[serde(tag = "on", rename_all = "snake_case")]`); `struct VmPlacement { repo: String /* pack repo key */, branch: Option<String>, size: Option<String>, mcp: Option<String> /* preset */, with: Option<String> /* box whose sandbox it shares: subsessions */ }` | all optional fields default | — |
| `launch.rs` | `PLACEMENT_ENV = "ATELIER_PLACEMENT"`, `attach_placement`, `take_placement` (as `GROUP_SKILLS_ENV`); `vm_agent(kind) -> "vm:copilot"`, `parse_vm_agent`; `agent_launch` sets both for a VM node | JSON in `AgentLaunch::env` | an older service refuses `vm:copilot` ("unknown agent") |
| `vm.rs` (new) | `VmView`, `VmInfo`, `VmState { Queued, Claiming, Starting, Running, Suspending, Suspended, Resuming, IntegrationOffline, Lost, Deleting, Deleted, Failed }`, `PackSummary { name, version, repos: Vec<RepoSummary { key, folders: Vec<String> /* globs */, default_branch }>, sizes, presets }`, `PoolView`, `ImageView`, `BudgetView`, `QueuedStart`, `Refusal { ms, box, kind: mcp|egress, server, tool, why }`, `GitState { ahead, behind, dirty, untracked, stashes, branch }`, `PackStatus { ok, findings: Vec<Finding{level, file, line, text}> }`, `VmOp { Suspend, Resume, Shell, UpdateFromMain, Delete { confirm: Vec<String> }, CancelStart, RetryStart, Adopt, Forget }`, `CliOp` (§4), `OpAnswer { ok, text, state: applied|queued|refused, data, outputs: Vec<(String, Vec<u8>)> }`, `ControlOp`, `sync_name_ok(rel: &str) -> bool` (components each `notebook`-plain: no `.`/`..`, no absolute, no device names, no `:`, no trailing dot or space, no control characters), `repo_for_folder(&[RepoSummary], path, PathStyle) -> Option<String>` (glob match), `branch_for_todo(id, title) -> String` (`atelier/DEC-031-short-title`, git-ref safe), `estimate_cost(size, ms, rates)` | JSON | — |
| `bus.rs` `Snapshot` | `vm: Option<crate::vm::VmView>` (filled in both `passive.rs:bus_loop` and `passive.rs:snapshot`; absent: no pack, VM features hidden) | `default, skip_serializing_if = Option::is_none` | ignores it |
| `todos.rs` `AgentPolicy` | `start_on_vm: bool` (board policy *Start on VM*) | `default, skip_serializing_if = Not::not` | an older service drops it when it rewrites the book: needs a current service (documented) |

`Node` has no `Default`: the compiler lists every literal (demo, tests, self-test, bench drivers).

### 2.6 Settings and environment

| Name | Where | Meaning |
|---|---|---|
| `settings.json` `vm.pack` | `canvas-common/src/settings.rs`: `vm: VmSettings { pack: Option<String> }` (`default, skip_serializing_if`) | the pack folder on the service's machine |
| `ATELIER_VM_PACK` | env | overrides `vm.pack` |
| `ATELIER_PLACEMENT` | `AgentLaunch::env` carrier | JSON `VmPlacement`; taken out by the service (any case) before anything starts |
| `ATELIER_REMOTE=satellite` / `satellite-lite` | guest sessions' environment (set by `SatSpawn` / the hooks lane) | the agent CLIs relay through the satellite |
| `ATELIER_SATELLITE` | guest sessions' environment | the satellite's local endpoint (Unix socket path; a pipe name under `vm::local` on Windows) |
| `ATELIER_VM_TEST_*` | test-only knobs of `vm::local` (clock, failure injection) | never read outside `vm::local` and the probes |

### 2.7 Stores under `<home>/vm/` (service is the single writer; atomic writes; `version` field; a newer or unreadable file is shown, never written)

| File | Holds |
|---|---|
| `hub.json` | this hub's id (UUID): the `atelier.hub` label on every sandbox it makes |
| `launches.json` | durable launch intents: id, box (terminal id), canvas id, `VmPlacement`, the `AgentLaunch` (JSON), state, fence, sandbox id, generation, attempts, error, timestamps |
| `sandboxes.json` | sandboxes known to this hub: id, provider, labels, role (`pool`, `session`, `builder`), image, size, state, lease (box ids), satellite id, git summary, cost accumulators, last seen |
| `satellites.json` | satellite credentials: id `sat-…`, sandbox, created, last seen, revoked, `secret` and `revoked_key` sealed with `canvas_sys::secret` |
| `images.json` | images per repo: provider id, built, base, satellite build, keep-N bookkeeping, last build log path |
| `sessions.json` | the `SessionStore` index: agent session id → { agent, sandbox, mirror root (relative to `<home>/vm/mirrors`), box }; read by `LocalBackend` too |
| `mirrors/<sandbox>/<root>/…` | history mirrors, same layout as the agent's own root (Copilot: `<id>/events.jsonl`, `<id>/workspace.yaml`; Claude Code: `<project dir>/<id>.jsonl`, `subagents/…`) |
| `ops/<sandbox>.json` | the last 1,000 relayed op ids and their answers (a replayed op gets its first answer) |
| `budget.json` | per-day running core-hours and estimated cost |
| `refusals.jsonl` | MCP and egress refusals (bounded 5 MB → `.old`), shown in the VM details |
| `bundles/<sandbox>/…` | built bundles waiting for down-sync (plugin, group skills, profile, instructions, MCP config) |

Guest side (`<guest home>` = `~/.atelier` of the guest user): `satellite.json` (hub URL,
credential id and secret, written 0600 at claim), `vm/outbox/` (durable ops), `vm/ring/` (none:
rings are memory), `bus/routing.json` and `canvas-agents/<canvas>/todos/book.json` (mirrored, for
local reads), `plugin/`, `group-skills/`, `profile/`, `notes/<group>/` (rw-notes mirror),
`mcp/` (agent MCP configs), `cli/atelier` (on `PATH`).

---

## 3. Phases

Each work item has an id (`VM-<phase><n>`, used as the commit prefix, as `WI-0xx` is today), the
files, the types, the tests and its share of the estimate. "Lead" items touch hot files (§6.2);
"worker" items own new files only. Every phase ends with the verification of `canvas-verify` for
what it touched (§6.4) and the docs it changed.

### Phase F: foundations (after the rename lands) — 1.5–2 days, lead

Goal: the types every later phase codes against, merged early so workers can start in parallel.

| Id | Work | Files | Tests |
|---|---|---|---|
| VM-F1 | Satellite protocol enums, `CAP_SATELLITE`, `ClientMsg::Sat` / `ServiceMsg::Sat` at 29, shaping | `canvas-common/src/sat.rs` (new), `protocol.rs`, `lib.rs` | index pins, shaping, frame sizes (§2.1–2.2) |
| VM-F2 | `Credential::Satellite`, `Accepted::satellite`, pipe refusal | `canvas-common/src/handshake.rs` | handshake tests: a satellite credential over TCP is accepted with `satellite: true`; over the pipe refused; revoked satellite gets a verifiable refusal |
| VM-F3 | Model types: `Placement`, `VmPlacement`, `vm.rs` (views, `CliOp`, `OpAnswer`, `ControlOp`, name checks, glob repo matcher, branch naming, cost), `Snapshot::vm`, `AgentPolicy::start_on_vm`, launch carrier and `vm:` agent prefix, `rpc.rs` requests and `is_service_state` | `canvas-model/src/{doc,launch,vm,bus,todos,rpc,lib}.rs` | old JSON loads and unset fields are not written (pattern `agent_fields_default_for_old_canvases_and_are_omitted_when_unset`); carrier taken out in any case and dropped when malformed (pattern of `take_group_skills`); `vm:copilot` refused by `AgentKind::parse`; `sync_name_ok` table (traversal, absolute, `C:`, `nul.md`, `GIT~1`, trailing dot, control chars, links are checked by the receivers); `repo_for_folder` on both path styles; `branch_for_todo` ref-safe |
| VM-F4 | `settings.json` `vm.pack`, `ATELIER_VM_PACK` | `canvas-common/src/settings.rs` | round trip, unset not written |
| VM-F5 | Module skeletons with `todo!()`-free stubs and the `vm::Vm` handle in `Agents` (no behaviour), `[dependencies]` for `reqwest`/`rustls` in `canvas-agents` (same per-OS TLS features as `canvas-push`) | `canvas-agents/src/vm/mod.rs` (new), `service/mod.rs` (one field), `Cargo.toml` | builds on the portable clippy line and `cargo xwin clippy` |

Exit: `cargo clippy --workspace --all-targets -D warnings` (macOS, `--no-default-features`
locally; Windows by `cargo xwin clippy`), the portable-crate clippy line, `cargo test -p
canvas-common -p canvas-model`, and CI green on the branch.

### Phase M: the thin lane — 5–7 days (design 4–6)

Goal: a box can run on a VM today through the pack's `hooks/`, without the satellite link: the
hub's terminal runs a provider shell into the guest (typically `aca sandbox shell … tmux new -A
-s BOX '<agent>'`), and `atelier satellite --lite` in the guest is polled over `exec` for history
and CLI operations. It is the first thing the owner can try on Azure, and its measurements
re-estimate the rest. Nothing here is thrown away: the hooks lane becomes the `vm::hooks`
provider (D12) and the `CliOp` executor is Phase 3's.

| Id | Work | Files | Types / behaviour | Tests |
|---|---|---|---|---|
| VM-M1 (worker A) | Pack loader v1, every file of §5 of the design parsed (unused parts validated only), placeholders `{{repo.path}}`, `{{secret:NAME}}`, `{{box.branch}}`, `{{box.repo}}`; redaction (values never in `Display`/`Debug`/logs); `atelier vm pack check` (findings only, never values; warns on a pack inside a git work tree with a public remote; shows the commands of `secrets.yaml` and `hooks/`) | `canvas-agents/src/vm/pack.rs`, `vm/pack/{schema,yaml,check}.rs` (new), `docs/examples/vm-pack/**` (new, `contoso`/`example.com` only) | `Pack`, `PackError`, `PackStatus`; `pack_version: 1`; unknown keys are findings, not errors | parse every example file; each schema error has a file and line; `Debug` of a loaded pack contains no secret value (a test greps it); the public-remote warning in a temporary repository |
| VM-M2 (worker B) | Hooks runner: `claim`, `shell-command`, `exec` (new, D12), `suspend`, `resume`, `delete`, `image`; JSON on stdin and stdout, a timeout each, stdout/stderr bounded, never logged beyond their first line and length; a stand-in `atelier vm-hook-standin` that implements every hook against a local "guest" folder (it is `vm::local`'s guest in hook form) | `canvas-agents/src/vm/hooks.rs` (new), `src/vm_hook_standin.rs` (new), `src/main.rs` (dispatch, hidden) | `HookCall`, `HookResult`; exit codes mapped to errors with the hook's first line | every hook round trip with the stand-in; a hanging hook times out; a hook printing a secret-looking value is not logged |
| VM-M3 (lead) | Placement in launches: the client sends `vm:<kind>` + `ATELIER_PLACEMENT` for a VM box (and skips the hub `PathInfo` check); `resolve_agent` takes the placement (carrier, else the document's node via `Docs`), refuses a local launch of a VM box, and in hooks mode resolves to a local terminal running `atelier vm attach --box ID --intent N` | `canvas-model/src/launch.rs`, `canvas-core/src/app.rs:1590` (`launch`), `canvas-agents/src/service/mod.rs` (`resolve_agent`, a `placement_of(id)` helper over `docs`), `service/docs.rs` (lookup of a node by id) | `vm::Placement` resolution order: carrier > document > local | service unit tests: carrier wins; document placement without carrier → refused "update this client"; malformed carrier dropped and the launch refused (never local); `tests/model_launch.rs` follows a VM box to `atelier vm attach` |
| VM-M4 (lead) | `atelier vm attach`: writes the intent (`launches.json`, minimal), runs `claim` (progress printed in the terminal), then `shell-command` with the PTY inherited (a child on Windows, `exec` on Unix); on exit prints how to resume | `canvas-agents/src/vm/attach.rs` (new), `src/main.rs` | intent states `claiming → attached → ended` | with the stand-in: the terminal shows the stand-in agent; killing the shell leaves the guest's tmux session (stand-in: a detached process) and a second attach re-joins it |
| VM-M5 (lead) | `atelier satellite --lite`: in the guest, `--drain --state JSON` prints history appends (Copilot `events.jsonl` and `workspace.yaml`, by offsets and file identity) and spooled ops; `--answer JSON` writes op answers; the guest CLI in `ATELIER_REMOTE=satellite-lite` spools a `CliOp` into `~/.atelier/vm/outbox/` and waits up to 5 s for its answer, else prints *queued (applied when the hub polls)* | `canvas-agents/src/vm/satellite/lite.rs`, `vm/satellite/outbox.rs`, `vm/satellite/client.rs` (new) | `CliOp`, `OpAnswer` (VM-F3) | spool and answer files are atomic; a drain is idempotent for the same state |
| VM-M6 (lead) | Hub poller: every `poll_seconds` (pack, default 10) per attached box, `exec` → drain → append into `<home>/vm/mirrors/…` → register in `sessions.json` → apply ops through the `CliOp` executor as the box → `exec` → answer | `canvas-agents/src/vm/lite_poll.rs`, `vm/ops.rs` (the executor, first version: `emit`, `send`, `status`, `effect`, `todo`, `pr`, `report`; see §4), `cli_io.rs` (the `Io` seam, first for `todo`, `report`, `pr`; Phase 3's VM-31 completes it) | executor runs the existing CLI functions with `home = hub home`, identity forced to the box | ops from another sandbox's box are refused; a replayed op id returns its first answer; history appends are refused when `file` fails `sync_name_ok` or the offset does not continue the mirror |
| VM-M7 (lead) | Status from mirrors: `index_loop` gets one Copilot `Index` per mirror root (from `sessions.json`), liveness overlay = "the hub terminal of this box is alive" in the lite lane | `index.rs` (liveness hook), `service/mod.rs:index_loop` | `Index::with_liveness(Box<dyn Fn(&str) -> Option<bool>>)` | index tests: a mirrored `inuse.*.lock` never counts; the overlay decides `live` |
| VM-M8 (worker A or lead) | Minimal UI: *Run on* row in the new-session dialog (`this machine` / `VM`), shown only when the snapshot has `vm`; the repo chosen from the folder (`repo_for_folder`) or from a list; a `☁` marker on VM boxes; Tasks *Start on VM…* (the dialog prefilled with *Run on: VM*, branch `branch_for_todo`); the snapshot's `vm.pack` summary | `canvas-core/src/dialogs.rs` (NewSession: `run_on`), `ui_modals.rs`, `modals.rs`, `ui_canvas.rs`/`view.rs` (`BoxModel::vm`), `tasks/flows.rs`, `tasks/detail.rs`, `canvas-agents/src/service/passive.rs` (fill `view.vm`) | `RunOn { Local, Vm(VmPlacement) }`, `BoxModel::vm: Option<VmBadge>` | view snapshots `new-session-run-on-vm`, `vm-box-marker`, `tasks-start-on-vm`; nothing launches while viewing |
| VM-M9 (lead) | Docs: README (Features: *VM sessions (thin lane)*; Commands: `vm attach`, `vm pack check`, `satellite --lite`); VM-SESSIONS.md *As built* section; AGENT-PROVIDERS.md (VM launches); `docs/VM-OWNER-CHECKLIST.md` (M items) | docs | — | — |

Tests that run in CI (all three OSes): `selftest vm --lane hooks` (new, `src/vm_probe.rs`): a
service in a temporary home listening on `127.0.0.1:0`, the example pack with the hook stand-in, a
VM box made through the protocol client, attach, the stand-in agent writes Copilot history and runs
`atelier todo prime`, `todo claim`, `emit`, `report publish --message` from inside the "guest";
the mirror makes the box *working* then *idle*; the claim is in the book as the box; the report is
in the inbox as the box; an older-client launch (no carrier) is refused.

Owner (Azure, §8): M-1 the aca hook scripts of the example pack against a real group; M-2 an
agent CLI authenticated in a guest (S2's first answer); M-3 first-prompt latency through hooks.

**Exit:** the CI probe passes on Windows, macOS and Linux; the owner's M-1…M-3 have been run once
(not a merge blocker; their numbers re-estimate Phases 1–5); docs updated.

### Phase 0: spikes — 2–3 days of ours; the runs are the owner's

The spikes need real sandboxes, which this machine does not have. Phase 0 therefore produces a
**kit** the owner runs (`atelier vm spike …`, each printing a JSON result with no pack values), a
local analog of each that runs in CI, and the checklist. Each spike command lands with the phase
that makes it possible; the owner runs them as they arrive and the results feed the re-estimate.

| Spike | What it answers | Kit command (lands in) | Local analog in CI |
|---|---|---|---|
| S1 | A satellite in a guest reaches the hub's WebSocket listener through the Dev Tunnel's web forwarding with the token injected by egress (TLS interception? `HTTPS_PROXY`? header on the upgrade?); the role is decided before any terminal list or document | `vm spike s1` (Phase 2): creates one sandbox, uploads the satellite, measures connect time, checks that the first frame after `Welcome` is a `SatDown`, never `Ready8`/`Terminals` | `selftest vm` step "a satellite through the web-forwarding stand-in gets `Welcome` and no client message"; handshake unit tests |
| S2 | The agent CLI authenticates in a guest: with egress `Transform` alone, or a token after claim (which, scoped how) | `vm spike s2 --agent copilot|claude|opencode` (Phase 1): runs `<agent> --version` and a no-prompt start in the guest with the pack's auth plan, reports what it needed | none (needs a real CLI and account) |
| S3 | The Linux `atelier` build runs in the stock image (glibc, libs), and a PTY + alacritty work there | `vm spike s3` (Phase 1): uploads the binary, runs `atelier satellite --self-check` (PTY echo, emulator, socket) | `atelier satellite --self-check` in the Linux CI job |
| S4 | End-to-end first prompt p50/p95 (target ≤ 5 s / ≤ 10 s) from a warm pool | `vm spike s4 --runs 20` (Phase 2) | `selftest vm` measures the same path on `vm::local` (a lower bound) |
| S5 | MCP: policy filtering and one process per guest session with a real server (initialize, notifications, cancellation, server-initiated requests) | `vm spike s5 --server NAME` (Phase 3) | gateway tests with `atelier mcp-standin` |
| S6 | The user's data-handling policy for the pack (what may leave the hub: notes, history, skills) | documented as a pack prerequisite in `docs/VM-OWNER-CHECKLIST.md`; `pack check` asks for `policy_ack: <date>` in `vm.yaml` | — |
| S7 | Failure injection: suspend during output, resume, network loss, sandbox deleted under the hub, preview API errors (429, 5xx) | `vm spike s7` (Phase 2/5) | `selftest vm --faults` (drops, cut frames, freeze, kill, provider errors from `vm::local`) |

Exit: kit commands exist for S1–S5 and S7, each with its local analog green in CI; the checklist
and S6 text are in `docs/VM-OWNER-CHECKLIST.md`.

### Phase 1: the manager — 7–9 days (design 5–6)

Goal: sandboxes, images, a warm pool, budgets and durable launches behind a provider trait, with
`vm::aca` built against the documented REST API and `vm::local` running everything in CI.

| Id | Work | Files | Types / behaviour | Tests |
|---|---|---|---|---|
| VM-11 (lead) | `vm::Provider` trait | `canvas-agents/src/vm/provider.rs` (new) | `trait Provider: Send + Sync { fn create(&self, &SandboxSpec) -> Result<Sandbox>; fn get(&self, id) -> Result<Option<Sandbox>>; fn list(&self, &Labels) -> Result<Vec<Sandbox>>; fn exec(&self, id, &ExecSpec) -> Result<ExecResult>; fn put_file(&self, id, path, &[u8], mode) -> Result<()>; fn get_file(&self, id, path, max) -> Result<Vec<u8>>; fn suspend(&self, id) -> Result<()>; fn resume(&self, id) -> Result<()>; fn delete(&self, id) -> Result<()>; fn set_egress(&self, id, &EgressPolicy) -> Result<()>; fn egress_decisions(&self, id) -> Result<Vec<EgressDecision>>; fn image_build(&self, &ImageSpec) -> Result<ImageRef>; fn image_commit(&self, id, name) -> Result<ImageRef>; fn image_delete(&self, &ImageRef) -> Result<()>; fn images(&self) -> Result<Vec<ImageRef>>; fn secret_set(&self, name, key, value: &Secret) -> Result<()>; fn stats(&self, id) -> Result<SandboxStats>; fn name(&self) -> &'static str; }`; `SandboxSpec { image, size, labels, egress, env_public }` (never secrets in `env_public`); `Secret` zeroized on drop, no `Debug` | trait object tests through `vm::local` |
| VM-12 (worker A) | `vm::aca`: REST client for the data plane (Appendix A): group path, create (`sourcesRef`, `resources`, `lifecycle.autoSuspendPolicy.enabled=false`, labels), list by labels, get, `stop`/`resume`, `executeShellCommand`, files (`PUT/GET …/files?path=`), `egresspolicy`, `egress-decisions`, `commit`, `diskimages`, `secrets/{id}` (`{"values": {…}}`), `stats`, `delete`; polling helpers with bounded waits; retry with jitter on 429/5xx honouring `Retry-After`; `api-version` and endpoint from the pack | `canvas-agents/src/vm/aca/{mod,rest,model,token}.rs` (new) | `token.rs`: `az account get-access-token --resource <audience> -o json` (or the pack's `auth.command`), cached until 5 min before expiry, refreshed on a background thread; never logged; launch path never waits for the CLI unless the cache is empty | an HTTP stand-in server (in-process, `std::net::TcpListener`) replays recorded shapes: create→poll→running, 429 with `Retry-After`, 409 on resume of a running sandbox, a 5xx burst, malformed JSON; bodies match the SDK's (golden JSON per call); a test that the bearer token never appears in errors or logs |
| VM-13 (worker B) | `vm::local`: sandboxes as folders under a provider root (`<root>/sandboxes/<id>/{home,work}`, `state.json`), images as folders (`<root>/images/<id>/` holding a seeded clone made from a local bare repository and the satellite path = the current executable), `exec` = a child with `HOME`/`ATELIER_HOME`/`cwd` in the sandbox and the stand-in proxy in `HTTP(S)_PROXY`, files = plain file I/O with containment, `suspend`/`resume` = freeze/thaw the sandbox's whole process tree (`canvas_sys::process::{suspend_tree, resume_tree}`), delete = kill tree + remove, labels in `state.json`, secrets held by the egress stand-in (never under the sandbox), failure injection knobs (`ATELIER_VM_TEST_FAIL=create:1,resume:2`, latency, a deleted-behind-your-back sandbox) | `canvas-agents/src/vm/local.rs` (new), `canvas-sys/src/process/{mod,unix,windows}.rs` (suspend/resume) | `canvas_sys::process::suspend_tree(pid) -> Result<usize>` (Unix: SIGSTOP each pid of `process_tree`, root last; Windows: `NtSuspendProcess` per process, or `SuspendThread` over a thread snapshot), `resume_tree` | freeze a `ping`-like child: no output while frozen, output resumes; a frozen satellite's link is dropped by the hub after 6 s and replays after thaw; secrets grep: no value under the sandbox folder or in the satellite's environment |
| VM-14 (lead) | `vm::store`: the `<home>/vm/*.json` files of §2.7 (versioned, atomic, never written over when newer/unreadable) and the satellite credential store (`sat-` ids, sealed secrets, revocation keys) | `canvas-agents/src/vm/store.rs` (new) | `Store::load_checked`, `Satellites::{mint, credential, revoke, seen}` | pattern of `a_book_that_cannot_be_read_is_never_overwritten`; secrets never in clear on disk (pattern of `remote.rs` tests) |
| VM-15 (lead) | `vm::Manager`: images (schedule from `vm.yaml`, default nightly, keep N, build = create from base → upload satellite (D5) → run the pack's image script via exec → clone at the default branch with egress for git → commit; the builder uses the smallest size the image allows: the platform keeps the builder's disk size as the image's minimum), warm pool per (repo, size) with minimums, leases, max age (12 h) and newer-image recycling (members are never returned to the pool after a claim), budgets and quota (max running, max cores, daily soft budget: starts queue with the reason; running work is never killed), `suspend_after` bookkeeping (Phase 4 acts on it) | `canvas-agents/src/vm/manager.rs` (new), `vm/images.rs`, `vm/pool.rs`, `vm/budget.rs` (new) | decisions are pure functions over `(state, pack, now)` returning actions (`Create`, `Delete`, `Recycle`, `Queue(reason)`), applied by one `vm` thread; the pattern of `remote.rs`'s `*_at(now)` functions | pure tests for every rule at given times (pool refill, recycle at max age and at a newer image, keep-N, budget queueing and the message, quota in cores); a `vm::local` integration test runs a day of schedule with an injected clock |
| VM-16 (lead) | `vm::Launcher`: durable intents in `launches.json` and the idempotent state machine `queued → claiming → credentials → bundles → spawn → attached` (+ `failed`, `canceled`, `rolled_back`), with a fence per intent (a stale worker's step is a no-op), cancel/retry, rollback (a claim that failed after credentials revokes them and recycles the member), reconciliation at start (sandboxes labelled `atelier.hub=<id>`: adopt those in `launches.json`; delete orphan pool members and builders; list orphan session sandboxes in the view with *Adopt* / *Delete…*, never deleted automatically) | `canvas-agents/src/vm/launcher.rs` (new) | `Intent { id, box, canvas, placement, launch: AgentLaunch, state, fence, sandbox, generation, attempts, error }` | state machine table tests (every edge, crash between any two steps then restart, double claim impossible, cancel during each step); reconciliation tests on `vm::local` |
| VM-17 (lead) | Claim: resume the member, `ControlOp::Prepare { repo, base, branch }` (Phase 1 via provider `exec` running `atelier satellite --prepare`, Phase 2 through the link): fetch the default branch, create the branch from it or track an existing remote branch, never rebase a running branch; mint the satellite credential and write `satellite.json` (0600) with `put_file`; set the sandbox's egress from the repo's profile with placeholders filled | `vm/launcher.rs`, `vm/egress.rs` (render only; the engine is Phase 3) | — | on `vm::local`: a claim makes the branch in the seeded clone, the credential file is 0600 (Unix), the egress policy the stand-in got is the rendered template |
| VM-18 (lead) | `VmView` in the bus snapshot (both places) and `VmInfo` per box; the `Vm*` requests answered by the service; `atelier vm list|pool|images|image build REPO|budget|pack check` over the service connection (like `remote clients`) | `service/passive.rs` (two lines), `service/mod.rs` (`is_service_state` arm), `src/vm_cli.rs` (new), `src/main.rs` | quantized fields (uptime, cost in whole minutes / cents) so the snapshot does not change every second | `backend_contract`: service answers over loopback, `LocalBackend` refers to the service; snapshot field absent without a pack |
| VM-19 (lead) | `has_work()` includes pending intents, image builds and pool upkeep; `doctor` gains a VM section (pack, provider auth cached, remote listening and tunnel web URL known, satellite binary SHA-256, pool) | `service/mod.rs`, `src/selftest.rs:doctor` | — | doctor output in a sandbox home with the example pack |

**Exit:** on `vm::local` (Windows, macOS, Linux CI): images build and rotate, the pool fills and
recycles, a launch intent survives a service kill at every step and completes after restart,
budgets queue starts with a reason, orphans are adopted or listed; `vm::aca` passes against the
HTTP stand-in; `pack check` on the example pack is clean; `atelier vm list` works locally and over
`--remote loopback`. Owner: `vm spike s2`, `s3` on Azure.

### Phase 2: terminals — 10–13 days (design 7–9)

Goal: a VM box is a live terminal drawn on the hub with its PTY in the guest, surviving link drops,
hub restarts and suspends; its history is mirrored and every history consumer resolves it.

| Id | Work | Files | Types / behaviour | Tests |
|---|---|---|---|---|
| VM-21 (lead) | **Remote PTY in `Terminal`** | `canvas-host/src/term.rs` | `pub trait RemotePty: Send + Sync { fn accepting(&self) -> Result<()>; fn input(&self, bytes: Vec<u8>) -> Result<()>; fn resize(&self, cols: u16, rows: u16); fn signal(&self, sig: Signal); fn stop(&self, force: bool); }` (`accepting`: Ok when linked, an error "VM offline: input not sent" when the link is down, Ok while a suspended VM resumes because the link buffers, D9); `Terminal::spawn_remote(spec: RemoteSpawn, hooks, Arc<dyn RemotePty>) -> (Arc<Terminal>, RemoteFeed)`; `RemoteFeed { fn output(&self, bytes) -> FeedResult; fn exited(&self, code, error: Option<String>); fn reset(&self, vt: Vec<u8>); fn note(&self, text) }` (`note` writes a dim status line into the emulator: *claiming a VM…*, *output skipped while offline*); a new `Ctl::Reset(Vec<u8>)` recreates the engine at the current size and feeds the repaint; `pid = 0`, `master = None`, `killer` = a `RemoteKiller: ChildKiller` calling `stop(true)` so `force_kill` and the graceful `stop()` (Ctrl+D, Ctrl+D, Ctrl+C, kill) work unchanged over the link; `Sink::reply` is a no-op for remote terminals (D7); no `cwd` existence check; `Terminal::is_remote()`; `send_input`, `click` and `wheel` call `accepting()` first for remote terminals, so a refusal reaches the client as `ServiceMsg::Error` (the path `dispatch` already uses); the input channel is drained by a remote writer thread that calls `RemotePty::input` | unit tests with a fake `RemotePty`: output reaches snapshots, input goes out encoded by the hub's modes, resize and stop map to the trait, a reset replaces the screen and keeps the size, replies are not sent, exit sets codes like the waiter; the existing `term.rs` tests unchanged |
| VM-22 (worker A) | **VT repaint from a snapshot** (the gap reset) | `canvas-host/src/emulator/replay.rs` (new) | `pub fn repaint(snapshot: &Snapshot) -> Vec<u8>`: RIS, modes (`?1000/1002/1003/1006/1005/2004/1/1004/25`, keypad, kitty flags `CSI = f ; 1 u`), alternate screen when active, each row's runs with SGR (colours, flags, underline style and colour), OSC 8 links, then cursor position and shape (DECSCUSR); `Snapshot` gains nothing | round trip for both engines: feed every corpus entry (`canvas-corpus`) into engine A, repaint into a fresh engine B, compare cells, modes and cursor (alacritty→alacritty, ghostty→alacritty when built, alacritty→ghostty); wide and combined cells; soft-wrap markers |
| VM-23 (worker A) | **The satellite** (guest side) | `canvas-agents/src/vm/satellite/{mod,link,dial,pty,ring,history,endpoint,outbox,proc,stats,files}.rs` (new), `src/main.rs` (`satellite` subcommand) | `link`: reads `satellite.json` (waits for it in a pool member), dials (`dial.rs`: `ws://` direct or through `HTTP_PROXY` absolute-form with `Upgrade`; `wss://` with rustls + platform roots + optional pack CA, through `HTTPS_PROXY` CONNECT), `handshake::open` with `remote_id = sat-…` and `CAP_SATELLITE | CAP_COMPRESS`, `SatUp::Hello`, then a reader and a two-lane writer (terminal I/O first, then everything else, `select_biased!`), reconnect 0.5 s, 1 s, then every 1.5 s, heartbeats; `pty`: each terminal is a `canvas_host::term::Terminal` (local PTY, **alacritty**, D6) whose raw output also feeds the ring through a new `Hooks::raw` tap; `SatDown::Input` → `Input::Raw`, acked when queued for the PTY writer; generations: one live generation per terminal id, a `Spawn` for a running term is refused; `ring`: per terminal 4 MiB of raw output with absolute offsets, 32 MiB per sandbox; `Resync { have }` replays from `have` when the ring still holds it, else `Snapshot` (the terminal's `snapshot()` repainted by VM-22); `history`: watches the roots the hub named in `Welcome` (Copilot `~/.copilot/session-state/<id>/{events.jsonl,workspace.yaml}`; never lock files), ships appends with `(file, ident = inode ^ ctime, offset)`, whole small files when rewritten, rate-limited (2 MiB/s per sandbox); `proc`: every 2 s, per terminal the agent's tree (alive), nested agent sessions (the existing `nested_copilots` + `name_node_copilots` on the guest's process table) and live sessions (lock owners alive, checked in the guest); `endpoint`: a Unix socket at `ATELIER_SATELLITE` (a pipe on Windows for `vm::local`) where the guest CLI sends `CliOp`s; the caller's terminal comes from the peer pid's ancestry, never from its environment; `outbox`: durable ops while the hub is away (`~/.atelier/vm/outbox`, ≤ 1,000 ops, ≤ 16 MiB); `stats`: CPU, memory, disk, git (ahead/behind/dirty/untracked/stashes) every 30 s | in-process tests: a satellite against a fake hub on loopback (spawn, echo, resize, signal, stop, exit codes, resync inside and beyond the ring, two generations fenced, input acked once, outbox survives a satellite restart); the dialer against a local HTTP proxy stand-in (absolute-form `Upgrade`, `CONNECT`) and a local TLS server with a test CA; `--self-check` |
| VM-24 (lead) | **Hub link** | `canvas-host/src/server.rs`, `canvas-agents/src/vm/link.rs` (new) | `serve_client`: after the handshake, `if accepted.satellite` → `host.app().serve_satellite(SatelliteConn { reader, writer, closer, info, websocket })` before `Ready8`; the satellite never enters `host.clients` (no broadcasts, no `visible`, no clipboard, no liveness of clients); requires `CAP_SATELLITE` else closes; `HostApp::serve_satellite` (default: refuse) and `HostApp::remote_pty(id, &Resolved) -> Result<Arc<dyn RemotePty>>`; `vm::link`: per sandbox one live link (a newer one replaces the old, as client sessions do), only `ClientMsg::Sat` and `Heartbeat` accepted (anything else closes it), every `term` checked against the sandbox's assigned terminals and the current generation (stale: dropped and logged), per-link limits (in-flight bytes, 8 MiB queued output per terminal → overflow becomes a `Resync`, MCP streams 32, ops 64 in flight, history 2 MiB/s), a two-lane writer, its own heartbeat and 6 s silence rule; `RemotePty` implementation: inputs numbered per (term, generation), unacked kept (≤ 1 MiB) and resent after a reconnect (the satellite dedupes by seq), refused while the link is down | link tests with an in-process satellite: role decided before anything else is written (the first frame after `Welcome` from the hub is a `ServiceMsg::Sat`); a satellite credential on the pipe refused; a satellite naming another sandbox's terminal is closed; a non-`Sat` message closes; flood limits; a revoked satellite stops reconnecting |
| VM-25 (lead) | `Host::launch` for VM targets, shutdown and re-adoption | `canvas-host/src/server.rs`, `canvas-agents/src/service/mod.rs`, `vm/launcher.rs` | `Resolved` gains `target: ExecTarget` (in-process); a VM target creates the remote terminal at once (`alive`, `pid 0`, a note line per Launcher step) and the Launcher sends `SatDown::Spawn` when attached; `Host::shutdown` stops local terminals and **detaches** remote ones; at start the Launcher re-creates remote terminals for attached intents (`Host::adopt_remote`) before clients connect, and satellites reconnect; the client's "restart the service with live terminals?" question counts local terminals only | restart the service with a VM box running on `vm::local`: the box is alive again within 3 s, the screen comes back by replay, nothing was stopped in the "guest" |
| VM-26 (lead) | **`ExecTarget`** and target-aware commands (Copilot first; Claude Code and OpenCode in Phase 5) | `canvas-agents/src/vm/target.rs` (new), `agents/mod.rs` (`Provider::command_for(&self, &ExecTarget, id, &AgentLaunch, &Integration) -> Result<Command>` with the current `command` as its `Local` case), `agents/copilot.rs` | `ExecTarget { Local, Vm { sandbox, style: PathStyle, home: String, roots: GuestRoots { plugin, group_skills, instructions, notes, mcp, cli }, program: String } }`; for `Vm`: program name from the pack (`copilot` on the guest `PATH`), paths joined with `style`, no hub files created, no hub environment read, `PATH` composed in the satellite (`SatDown::Spawn` env entry `+PATH=dir` prepends) | `tests/model_launch.rs`: a VM box's Copilot command line has guest paths only (`/`, under the guest home), no `C:\`, no hub `PATH`; the Windows-hub case is built on every OS with `PathStyle::Posix` |
| VM-27 (worker B) | **`SessionStore`** | `canvas-agents/src/vm/session_store.rs` (new), `index.rs`, `service/mod.rs:index_loop`, `backend.rs` (`Transcript`, `HasHistory`, `Fork`), `agents/copilot.rs` (`is_live`, `has_history` through the store), `service/passive.rs:write_context/write_canvas_context` (digests through the store) | `SessionStore::resolve(home, agent, session) -> Root { Local(PathBuf) | Mirror { sandbox, path } }` reading `<home>/vm/sessions.json` (file-backed, so `LocalBackend` resolves alike, D17); `Liveness` overlay fed by `SatUp::Proc` (in the service) and "not live" for mirrors in `LocalBackend`; `index_loop` polls one `History` per (agent, root); `Fork` of a VM session answers "forking a VM session is not available yet" (D13) | backend contract: `Transcript` of a mirrored Copilot session is the same locally and over loopback; `HasHistory` true for it; passive digest of a group with a VM box names its mirror; a mirrored lock file never makes a session live |
| VM-28 (lead) | Subsessions found in the guest | `vm/link.rs`, `service/mod.rs` | `SatUp::Proc` nested sessions → `ServiceMsg::Subsession` and mirror registration (the child's history root is the same guest root) | an in-guest `copilot -p` stand-in becomes a linked subsession box on the hub |
| VM-29 (lead) | Gap, reconnect and status rules | `vm/link.rs`, `service/passive.rs:idle` | after a reconnect: `Welcome` carries per terminal `(generation, have, input_acked)`; the satellite replays or sends a snapshot (the hub marks the gap with a note line); queued messages (`mailbox`) are typed in only once the terminal has been reconciled and its history caught up (`idle()` is false while the link is down or reconciling); input during a resume is buffered (D9) | probe: drop the link during output (exact replay, no duplicate, no loss); drop for longer than the ring (one gap marker, screen equal to the guest's); a message queued during the drop is typed after reconciliation, once |
| VM-2A (lead) | Docs and the wire tables | BACKEND.md (*VM sessions on the wire*), REMOTE.md (*Satellites*: role, credentials, revocation), AGENT-PROVIDERS.md (*ExecTarget*), DEVELOPMENT.md (code map: `vm/`) | — | — |

Tests in CI (three OSes): `selftest vm` (`src/vm_probe.rs`, no windows): a service in a temporary
home with `ATELIER_LISTEN=127.0.0.1:0`, the example pack switched to `provider: local`, the
stand-in agent (extended: with `ATELIER_FAKE_HISTORY=1` it writes Copilot `workspace.yaml` and
`events.jsonl` lines per typed line and reports OSC 9;4 progress); steps: launch a VM box through
the protocol client; type and see the echo; status *working* then *idle* from the mirror;
`Transcript` of the box; drop the link (replay), freeze the sandbox for 10 s (link dropped, thaw,
replay), drop beyond the ring (gap), restart the service (re-adopted, nothing stopped), stop the
box gracefully, revoke (delete) and see the satellite give up. `selftest vm --faults` adds the
fault-injecting proxy of `remote_probe.rs` between satellite and hub.

**Exit:** the probe passes on Windows, macOS and Linux; `corpus check` with the repaint round
trip; `selftest`, `selftest --remote loopback` unchanged (45 steps); the clippy and portable lines;
owner: `vm spike s1`, `s4`, `s7` on Azure.

### Phase 3: integration — 10–13 days (design 7–9)

Goal: inside the guest every canvas command, skill and instruction works as on the hub, within the
pack's rules: CLI operations, file sync with hub-assigned roots, the profile bundle and path map,
egress rendered from templates with secrets refreshed by the hub, and the policy-checked MCP
gateway.

| Id | Work | Files | Types / behaviour | Tests |
|---|---|---|---|---|
| VM-31 (worker C) | **The `Io` seam** in the agent-facing CLIs, so the same functions run on the hub with inputs from the guest | `canvas-agents/src/cli_io.rs` (new), `cli.rs`, `todos_cli.rs`, `notes_cli.rs`, `reports.rs`, `reviews_cli.rs`, `prs_cli.rs` | `trait Io { fn read(&mut self, source: &str, max: u64) -> Result<Vec<u8>> /* "-" = stdin */; fn write(&mut self, path: &str, bytes: &[u8]) -> Result<String>; fn now_local(&self) -> bus::Local }`; `FsIo` (today's behaviour, unchanged output); `RelayIo { inputs, outputs }` | every existing CLI test passes with `FsIo`; new tests run each verb with `RelayIo` and compare outputs |
| VM-32 (worker C) | **Satellite mode of the CLI** in the guest | `canvas-agents/src/vm/satellite/client.rs`, `src/main.rs` (`ATELIER_REMOTE=satellite` → `vm::satellite::client::dispatch` before the normal dispatch) | per §4: reads run locally on the mirrored `bus/routing.json` and `canvas-agents/<canvas>/todos/book.json` (stale note when the hub is away); writes become `CliOp` with file options inlined (`--body`, `--summary`, `--results`, `--notes`, `--draft`, `--markdown`, `--html`, `--md`, `import FILE`), paths in data options (`todo link --note/--file`) mapped guest→hub through the path map or refused; outputs (`todo export --canvas`, `todo mirror --out`) written in the guest; non-agent verbs (`service`, `remote`, `vm`, `automations`, `config`, …) refused with "not available inside a VM session" | for every row of §4: a guest-mode run against a fake satellite endpoint produces the same text as the hub mode; offline writes print *queued (hub offline since …)*, never success |
| VM-33 (lead) | **Op relay** on the hub | `canvas-agents/src/vm/ops.rs` (complete), `vm/link.rs` | `SatUp::Op { id, term, json }` → check term ∈ sandbox → dedupe in `ops/<sandbox>.json` → run on a worker (never the link reader) with `Caller/Identity { session: box }` and `RelayIo` → `SatDown::OpResult`; events it posts go through `bus::post` (inbox-grade, so `pr.merged` from a guest never finishes todos: `from_channel` stays false); per-sandbox rate limits | ops replayed after a satellite restart are applied once; an op naming another sandbox's terminal is refused; a guest `emit pr.merged` does not finish a todo |
| VM-34 (lead) | **File sync** | `canvas-agents/src/vm/sync.rs` (new), `vm/satellite/files.rs` | hub-assigned roots (`WelcomeInfo.roots`): `plugin` (the canvas plugin), `group-skills/<n>` (the launch's group skill plugin), `canvas-skills/<slug>`, `profile` (§ below), `instructions/<group>` (the group instructions file, rewritten with guest paths), `routing` (the routed canvas's `routing.json`, filtered to what a session may read), `todos` (the book), `mcp` (agent MCP configs), `notes/<group>` (`rw-notes`: the group's `ai-docs` and `human-outputs`, plus `GOALS.md`); down: `File` chunks with revisions, `Manifest` removes what the hub no longer has (inside the root only); up: `SatUp::Note` for `rw-notes` roots only, `base_rev` checked like `NoteWrite` (`None`: must not exist; conflict → `NoteResult` with the hub's text, the guest keeps its version as `<name> (conflict <date>).md`), the hub writes through `crate::notebook` with the notebook's containment rules and `roots` = that group's folder | traversal and link negatives on both sides (`../x`, `/etc/x`, `C:\x`, `nul.md`, `GIT~1`, a symlink in the guest mirror pointing out, a name with `\`); a note written in the guest appears in the hub notebook with provenance; a concurrent hub edit becomes a conflict, never a loss; a cross-sandbox write (another sandbox's root id) is refused |
| VM-35 (lead) | **Profile bundle and path map** | `canvas-agents/src/vm/profile.rs` (new), `vm/pack.rs` (`profile.yaml`, `paths.yaml`) | `profile.yaml` allowlist (e.g. `~/.copilot/skills/*`, `~/.claude/skills/*`, `~/.claude/agents/*`, instruction files), keys allowed from agent config, always-excluded paths (anything named like a credential, `*.pem`, `.env`, tokens), a size cap; `paths.yaml` host prefix → guest path with `ro-snapshot` / `rw-notes` / `none`; the generated guest instruction (`.canvas/vm-paths.instructions.md` in the instructions root) tells the agent where hub paths are | a file outside the allowlist is never bundled; an excluded name inside an allowed folder is skipped; the generated instruction lists only mapped prefixes; a skill with `requires: host` front matter stays on the hub (risk table) |
| VM-36 (worker B) | **Egress** rendering and the local engine; the secrets refresher | `canvas-agents/src/vm/egress.rs`, `vm/secrets.rs` (new), `src/egress_standin.rs` (new, hidden `atelier egress-standin`) | templates of `egress/*.yaml` → `EgressPolicy { default: Deny, rules: [{ match: { host, path, methods }, action: Allow|Deny|Transform { headers: [{ op, name, value_ref: { secret, key, format } }] } }] }` with placeholders filled at claim; `vm::aca` sends it as the platform's JSON (`defaultAction`, `rules`, `match`, `action.headers[].valueRef.secretRef`); the stand-in is an HTTP forward proxy enforcing the same policy (absolute-form and `Upgrade` for host+path+method+headers; `CONNECT` for host only) and injecting header values it holds; secrets refresher: each `secrets.yaml` command run as the user with a timeout, its stdout piped to `Provider::secret_set` (never logged, never on disk, zeroized), on its interval and on demand | rule evaluation table (first match, deny default, methods, path prefixes and globs); egress tests from a "guest" (a child of `vm::local`): a forbidden host and a forbidden method are refused and logged as refusals; an allowed forge call gets the header while the guest's environment and files never contain the value |
| VM-37 (worker A) | **MCP gateway** | `canvas-agents/src/vm/mcp/{gateway,policy,jsonrpc,server}.rs` (new), `vm/satellite/mcp_proxy.rs` (new), `src/mcp_standin.rs` (new, hidden `atelier mcp-standin`) | satellite: an HTTP server on `127.0.0.1:<port>` serving `POST/GET/DELETE /mcp/<name>` (MCP Streamable HTTP: JSON or SSE responses, `Mcp-Session-Id`), relaying JSON-RPC messages over `Mcp` streams; hub: per (guest session, server) a lazily started process from the hub's own agent configuration (stdio: command, args, env with credentials; or HTTP with headers), protocol version pinned by the pack; `policy.rs`: default deny over server + tool + action + arguments (`mcp-policy.yaml` rules with placeholders `{{box.branch}}`, `{{box.repo}}`), `tools/list` filtered, every `tools/call` validated (unknown tools and parameters refused), refusals answered as JSON-RPC errors and recorded (`refusals.jsonl`, the VM details); notifications and cancellations passed per policy; server-initiated requests (`roots/list`, `sampling/createMessage`, `elicitation/create`) refused unless allowed; processes end with the session (idle 10 min) | with `atelier mcp-standin` (stdio; tools `list_pull_requests`, `create_pull_request {draft, head, base, title}`, `update_pull_request`, `vote`, `add_comment`; logs every message it receives; checks a credential in its own environment): initialize, `tools/list` filtered to the allowed ones, a refused `vote` and `update_pull_request` never reach it, `create_pull_request` with `draft: true` and `head: {{box.branch}}` passes, with another head is refused, cancellation passes, a server-initiated request is refused, one process per session, none after the session ends, the credential never in the guest |
| VM-38 (lead) | Guest MCP configuration per agent (Copilot first): the allowed servers (policy ∩ the box's `mcp_off` ∩ the preset) as `http` entries pointing at the satellite | `vm/sync.rs` (`mcp` root), `vm/target.rs` | Copilot: the guest's `~/.copilot/mcp-config.json` (the guest is per session, so its user config is the session's); Claude Code and OpenCode in Phase 5 | the stand-in agent lists exactly the allowed servers |
| VM-39 (lead) | Docs: AGENT-PROVIDERS.md (*Inside a VM*: commands, notes, MCP), NOTEBOOK-TASKS.md (notes from VMs, Start on VM), the skills' texts gain one line ("in a VM session the same commands work; writes may answer *queued*") with the plugin version bump (1.4.0) and the skill contract test | `canvas-agents/src/cli.rs` (`write_plugin`, skill texts), `todos_cli::SKILL`, `notes_cli::SKILL`, docs | — | `effects_are_checked_and_posted_for_the_service` gains the new line's checks |

Tests in CI: `selftest vm` gains the integration steps: inside the guest, `todo prime/claim/note/
submit --summary FILE`, `note new --md -`, `report publish --markdown FILE`, `pr track URL`,
`emit`, `send` (the hub's answers, as the box); a note conflict; hub offline (the hub service
stopped): writes print *queued*, reads say *stale*, everything applies once when it is back; MCP
with the stand-in and the policy of the example pack; egress refusals.

**Exit:** the above on three OSes; the §4 inventory has a test per row; owner: `vm spike s5`.

### Phase 4: UX — 5–7 days (design 4–5)

Goal: every VM feature in both clients, locally and in remote mode, viewing starts nothing.

| Id | Work | Files | Tests |
|---|---|---|---|
| VM-41 (worker A) | New-session dialog *Run on*: this machine / VM, then repo (from the folder via `repo_for_folder`, or a pick list), branch (default from the box name or the todo id; an existing remote branch is tracked), size (the pack's), MCP preset; group and canvas defaults (*Session defaults for group…* gains *Run on*); shown only with `snapshot.vm`; subsessions of a VM box default to its sandbox (`with`) | `canvas-core/src/dialogs.rs`, `ui_modals.rs`, `modals.rs`, `side.rs` (defaults), `actions.rs` | snapshots `new-session-run-on-vm`, `new-session-vm-fields`, `group-defaults-run-on`; dialog unit tests (fields, Tab order, the repo found from the folder, an unknown folder asks for a repo) |
| VM-42 (worker A) | Box states: `☁` plus *starting*, *queued: budget*, *suspended*, *integration offline since …*, *lost*, *deleting*; status line and border; the TUI cell badge and the GUI's polish; the `tree` hook carries `vm` per box | `canvas-core/src/ui_canvas.rs`, `view.rs` (`BoxModel::vm`), `status.rs`, `canvas-gui/src/canvas.rs`, `canvas-gui/src/hooks.rs` | snapshot `vm-box-states` (every state in both render modes); glyph one-cell test (D16) |
| VM-43 (worker A) | Details panel VM section: size, image age, uptime, estimated cost, git ahead/behind/dirty, integration status, refusals; buttons *Suspend*, *Resume*, *Open shell* (a shell terminal box in the same sandbox), *Update from main* (a message to the agent: "fetch origin/main and merge or rebase as you see fit; do not push without asking"), *Delete…* (the barrier, VM-46) — each a window-only bus action | `canvas-core/src/ui_side.rs`, `side.rs`, `actions.rs`, `canvas-agents/src/service/mod.rs` (`ClientMsg::Bus` arm: `vm::action` before `passive::action`) | snapshot `vm-details`; actions are bus actions and start nothing except *Open shell* and *Resume* (explicit) |
| VM-44 (worker A) | Tasks: *Start on VM…* (dialog prefilled: Run on VM, branch `branch_for_todo`, first prompt "Run `atelier todo prime`, `atelier todo show ID`, claim it, and submit with `atelier todo submit ID --pr URL`"), the board policy row *Start on VM* (`AgentPolicy::start_on_vm`) making it the default of *Start a session on this…* | `canvas-core/src/tasks/{flows,detail,forms}.rs` | snapshots `tasks-start-on-vm`, `tasks-board-policy-vm` |
| VM-45 (worker A) | ☁ VMs panel: a toolbar button (only with a pack) opening `RightDetail::Vms`: sandboxes by state, the pool, images (age, *Build now*), quota and budget, queued starts (*Cancel*), orphans (*Adopt*, *Delete…*), pack status (findings), refusals | `canvas-core/src/vms.rs` (new), `ui.rs` (toolbar), `side.rs` | snapshot `vms-panel`; hint and help rows |
| VM-46 (worker B) | Lifecycle on the hub: `suspend_after` (idle and unseen: no client shows it, not working, nothing queued) → suspend; start, typing, a queued message or a follow-up resumes; the **quiescence barrier**: stop new work, `ControlOp::GitCheck` (every branch vs. its upstream, stashes, untracked, uncommitted; resume first if suspended), re-check merged/closed with `gh`/`az` (the PR monitor's runner, read-only), drain queues with acknowledgements (outbox, history to its end, notes up-synced, MCP closed), then delete and revoke; anything ambiguous asks (the client shows the findings and *Delete anyway* lists what is lost); v1 asks for every automatic deletion | `canvas-agents/src/vm/quiesce.rs` (new), `vm/manager.rs` | pure tests of the findings table; on `vm::local`: an unpushed commit, a stash, an untracked file each block with their text; a clean pushed branch deletes after one confirmation; budgets never kill running work |
| VM-47 (lead) | Client exclusions: idle auto-stop skips VM boxes (C16); folder and path links of VM boxes are guest paths (Ctrl+click shows "on the VM" with *Copy path*, never a hub open); the sidebar's folder rows show the guest path with `☁` | `canvas-core/src/app.rs`, `links.rs`, `ui.rs` | unit tests |
| VM-48 (lead) | Parity rows and the self-test step | `src/parity_vm.rs` (new), `src/parity.rs` (`late_rows` call), `src/selftest.rs` (one step against `Ui`) | rows below; the self-test step "a box Run on VM (local provider): its ☁ box starts, takes typing, shows *working* then *idle*, Suspend and Resume from its details; nothing starts by viewing" (TUI and GUI, also over `--remote loopback`): 46 steps |
| VM-49 (lead) | Docs: README (Features: *VM sessions*; Controls; Commands `vm …`), PARITY.md rows and *Last run*, GUI.md (`tree` fields), VM-SESSIONS.md *As built* | docs | — |

Parity rows (numbers after 56; checked on `vm::local` in a sandbox of their own, `late_rows`):

| Row | Feature | Check |
|---|---|---|
| 57 | *Run on: VM* in the new-session dialog | `vm_new_session_run_on`: the row appears only with a pack; repo found from the folder; *Create & start* → a ☁ box, *starting* then live; the service log shows the intent; the canvas file has `placement` |
| 58 | ☁ box states | `vm_box_states`: suspended (frozen sandbox), integration offline (link cut), lost (sandbox deleted behind the hub), each drawn and in the `tree`; typing while offline gives the notice; while suspended resumes and the keys arrive once |
| 59 | VM details and actions | `vm_details_actions`: Suspend, Resume, Open shell (a shell box in the same sandbox), Update from main (the message typed in when idle), Delete… with an unpushed commit (blocked, its text), then clean (deleted, credential revoked) |
| 60 | Tasks: Start on VM | `vm_tasks_start_on_vm`: the dialog prefilled; the session claims the todo as itself through the satellite; the badge shows |
| 61 | ☁ VMs panel | `vm_panel`: pool, images (*Build now*), quota, a queued start with its reason and *Cancel*, pack findings |
| 62 | VM boxes in remote mode | `vm_remote_loopback`: a GUI over `--remote loopback` starts, types into and suspends a VM box (everything through the service) |

**Exit:** view snapshots read and accepted; parity rows 57–62 pass with the full suite (one GUI
suite at a time); goldens unchanged unless a captured state shows a VM box (then re-blessed after
looking); hardening unchanged; self-test 46 steps in TUI, GUI and both over loopback.

### Phase 5: hardening — 7–9 days (design 5–7)

| Id | Work | Files | Tests |
|---|---|---|---|
| VM-51 (lead) | `selftest vm --real` (owner-only; never in CI; prints no pack values): pack check, auth, one image build (or the newest), a pool member, a box with the real agent CLI started without a prompt (as `selftest agent` does), link replay and gap, suspend/resume, CLI ops, an MCP call refused and one allowed, an egress refusal, the barrier, delete; a JSON report | `src/vm_probe.rs` (`--real`) | dry-run mode against `vm::local` in CI |
| VM-52 (lead) | Soak: `selftest vm --soak --minutes N` on `vm::local`: link drops, freezes, cut frames, hub restarts, guest satellite restarts, provider 429/5xx; exactly-once input, no lost or doubled op, replay vs. gap counts, memory and handle growth of hub and satellite | `src/vm_soak.rs` (new), reusing `soak.rs` parts | 20-minute run results in VM-SESSIONS.md (as REMOTE.md has) |
| VM-53 (worker B) | Resume on a new VM: for a *lost* box (or by choice), claim a new sandbox, create the branch from its remote (the work lives in pushed branches), upload the mirrored history into the guest's agent root, resume the conversation; refuse when the mirror is incomplete (its last offset vs. the satellite's last report) | `vm/launcher.rs`, `vm/sync.rs` | on `vm::local`: delete the sandbox behind the hub, *Resume on a new VM*, the stand-in sees its history |
| VM-54 (worker A) | **Claude Code on VMs**: target command (guest paths for `--plugin-dir`, `--add-dir`, `--append-system-prompt-file` from the `instructions` root, the private `--mcp-config` file written into the guest's `mcp` root), JSONL mirror of `~/.claude/projects/<dir>/<id>.jsonl` and `subagents/` (the project dir follows the guest's clone path, which is fixed per repo key so resumes on a new VM find it), liveness from the guest's `~/.claude/sessions/<pid>.json` reported in `Proc` | `agents/claude.rs`, `agents/claude_history.rs` (root parameter), `vm/satellite/history.rs` | model_launch for a Claude VM box; mirror and transcript tests |
| VM-55 (worker A or B) | **OpenCode on VMs**: its history is SQLite plus its TUI's HTTP server, which cannot be mirrored by appends (§5.10): the satellite runs the OpenCode history reader in the guest and ships summaries, subagent children and detected events (`SatUp::Stats` JSON `opencode` part, or `Proc`), the TUI server's busy/permission/question state from the guest (the satellite polls it on loopback with the password it got in `Spawn` env), transcripts on demand through `ControlOp::Transcript`; re-keying (`ses_` ids) learned in the guest and reported; resume on a new VM through `opencode export`/`import` | `agents/opencode.rs`, `agents/opencode_history.rs` (root parameter), `vm/satellite/opencode.rs` (new), `vm/session_store.rs` (a `Remote` root kind answered through the link) | a stand-in OpenCode in the guest (the fake with `ATELIER_FAKE_OPENCODE_DB`) drives status and re-keying; transcript over the link |
| VM-56 (lead) | Shared MCP processes (opt-in per server) only after the tests the design names (initialize, notifications, cancellation, auth context, server-initiated requests) pass with the stand-in in shared mode | `vm/mcp/gateway.rs` | the five tests per mode |
| VM-57 (lead) | Final docs: README *Performance* (first prompt p50/p95 on `vm::local` and the owner's Azure numbers), VM-SESSIONS.md as built, PARITY *Last run*, IOS-STATUS (the phone shows VM boxes as sessions; no VM actions there) | docs | — |

**Exit:** `selftest vm` and `--faults` on three OSes; the 20-minute soak passes; Claude Code and
OpenCode VM boxes pass their probes with stand-ins; owner: `selftest vm --real` once per agent.

---

## 4. The CLI contract inventory (design §4.4: "each command's file and liveness contract is inventoried first")

Commands an agent runs inside a session, from `src/main.rs` (USAGE and dispatch at 555–627) and
the modules they call. "Liveness" is how the command decides whether to wait for the service
today: `reports::service_alive(home)` reads `<home>/reports/service.json` (pid and start time)
and checks the process (`reports.rs:86`). "Identity" is how the caller is found today (the
environment the service sets: `ATELIER_SESSION`, `ATELIER_HOP`, `ATELIER_PASSIVE`,
`ATELIER_SESSION_AGENT`, `AUTOMATION_NAME`). In satellite mode the identity is the terminal the
satellite proves from the caller's process tree; the environment is ignored (a mismatch with the
proven terminal is refused).

Satellite mode column: **L** = runs locally in the guest on mirrored files; **R** = relayed as a
`CliOp` and run on the hub by the same function with `RelayIo` and the box's identity; **R/L** =
relayed when the hub is reachable, local (marked stale) when not; **Q** = queued durably when the
hub is away, answering *queued (hub offline since …)*.

| Command | Reads today | Writes today | Liveness / wait today | Satellite mode → `CliOp` |
|---|---|---|---|---|
| `events` | `bus/routing.json`; the group folder's `.canvas/events.yaml` (created by `bus::ensure_catalog` when missing: a write) | possibly the catalog | none | **R/L** `Events` (hub creates the catalog; offline: routing mirror only, "the event catalog is on the hub") |
| `emit TYPE [--data JSON] [--to NAME] [--group G]` | `routing.json` (`check_target`) | `bus/inbox/<ms>-<uuid>.json` (`bus::post`) | none: prints "Sent …" even without a service | **R+Q** `Emit { kind, data, to, group, hop }`; the hub re-checks the target as the box and posts with `source = box` |
| `send --to NAME TEXT` | `routing.json` | inbox `message` | none | **R+Q** `Send { to, text, hop }` |
| `status ok\|warn\|alert\|done HEADLINE` | — | inbox `passive.status` | none | **R+Q** `Status { state, headline }` (passive runs stay on the hub in v1, D13; harmless from a VM) |
| `followup --in D \| --at HH:MM NOTE` | — (computes `at_ms` with the local clock and time zone) | inbox `passive.followup` | none | **R+Q** `Followup { after, at, note }`: computed on the hub (the guest's clock/zone, often UTC, would move `--at 09:00`) |
| `effect KIND --on NAME [--note] [--kind]`, `effect clear` | — | inbox `canvas.effect` | none | **R+Q** `Effect { kind, on, note, only }` |
| `effect` / `effect list` | — | — | none | **L** (static text) |
| `report publish (--markdown F\|- \| --html F\|- \| --message T) [--title] [--kind] [--pr] [--link]… [--session] [--thread] [--source]` | the body file or stdin (≤ 2 MiB) | `reports/spool/<id>.json` (tmp + rename) | `service_alive`; waits ≤ 10 s for `items/<id>` or `spool/<id>.rejected` | **R+Q** `ReportPublish { args, body }` (body inlined; checks run in the guest first for a fast refusal, again on the hub); offline: *queued (hub offline): stored when the hub is back* |
| `report list [--unread] [--limit N]` | `reports/items/*/meta.json`, `state.json`, `spool/`, `service.json` | — | `service_alive` for the "not running" note | **R** `ReportList { unread, limit }`; offline: "the inbox is on the hub (offline since …)" |
| `pr track URL [--session] [--title]` | — (parses the link) | inbox `canvas.pr` with a request id | `service_alive`; waits ≤ 10 s for `bus/answers/<request>.json` | **R+Q** `Pr { op: track, … }` |
| `pr untrack URL`, `pr check [URL]`, `pr dispatch URL --key --event TEXT` | — | inbox `canvas.pr` | as above | **R+Q** `Pr { op, … }` (dispatch is for canvas-wide agents and you: refused for a session by the hub as today) |
| `pr list [--format json]` | `routing.json` (canvas id), `canvas-agents/<canvas>/prs/registry.json` | — | none | **R** `PrList { json }`; offline: unavailable |
| `review handoff --pr --author [--author-id] --results F\|- [--notes F] [--outcome] [--title] [--link]… [--session]` | results and notes files (≤ 8,000 chars results) | inbox `canvas.review` | `service_alive`; answer ≤ 10 s | **R+Q** `Review { op: handoff, …, results, notes }` (notes stay private: they cross the TLS link once and land in the hub's book as today) |
| `review message REVIEW (--draft F \| --sent --conversation --recipient \| --needs-you WHY \| --failed WHY)`, `review checked`, `review request`, `review done` | the draft file | inbox `canvas.review` | as above | **R+Q** `Review { op, … }` (canvas-wide agents run on the hub; a session gets the hub's refusal) |
| `review list [--format json]` | `canvas-agents/<canvas>/reviews/book.json` | — | none | **R** |
| `todo prime`, `boards [--tree]`, `list …`, `show ID`, `ready [--board]` | `routing.json`, `canvas-agents/<canvas>/todos/book.json` (`try_load`) | — | none | **L** on the mirrored routing and book (`routing` and `todos` roots), with "as of HH:MM (hub offline)" when the link is down; identical text otherwise |
| `todo add`, `propose`, `update`, `move`, `claim`, `release`, `note`, `check`, `link`, `relate`, `submit`, `accept`, `decline`, `done`, `cancel`, `reopen`, `delete`, `board add\|link\|unlink\|delete` | the book (to resolve boards and ids); `--body F\|-`, `--summary F\|-` | inbox `canvas.todo` with a request id (`ask_from`) | `service_alive`; answer ≤ 10 s (`answered`: a refusal exits non-zero) | **R+Q** `Todo { words, options, flags, inputs }` (files inlined); `link --note PATH \| --file PATH`: a guest path mapped to the hub through `paths.yaml`, else refused ("this path is only on the VM; write it as a note with `atelier note new`"); policy refusals come from the hub's book as today |
| `todo import FILE [--board] [--prefix]` | the file (≤ 4 MiB) | inbox | as above | **R+Q** with the file inlined (an agent is refused by the book's policy anyway) |
| `todo mirror --set FILE\|off` | — | inbox (the service's mirror path, a hub path) | as above | refused from a VM ("the mirror is a file on the hub: set it from the canvas") |
| `todo mirror [--out FILE]`, `todo export --canvas FILE [--board]` | the book | `--out` / `--canvas` file (`write_atomic`) | none | **L**: rendered from the mirrored book, written in the guest |
| `note path [--audience ai\|human]` | `routing.json` (group folder, notebook root) | creates the folder | none | **L**: the guest path of the group's `rw-notes` mirror (`notes/<group>/ai-docs` …), created in the guest |
| `note new --title T (--md F\|- \| --html F\|-) [--audience] [--folder REL] [--name FILE]` | routing; the body | the note file via `notebook::create_new` (never over a file), provenance from `ATELIER_SESSION_AGENT` / routing | none | **L+sync**: written into the `rw-notes` mirror with provenance (`source_session` = the proven box), printed as the guest path; up-synced with `SatUp::Note { base_rev: None }`; the hub's `NoteResult` gives the hub path (a name taken on the hub gets `-2` there and the guest file is renamed to match) |
| `note list [--audience] [--internal] [--format json]` | the group's `ai-docs` and `human-outputs` | — | none | **L** on the mirror (guest paths, mapped hub paths in JSON as `hub_path`) |

Commands refused inside a VM session (they act on the hub machine or its service): `service …`,
`remote …`, `vm …`, `automations …`, `config …`, `doctor`, `record`, `corpus`, `bench`,
`selftest`, the clients. Each says "not available inside a VM session (it acts on the canvas's
machine)".

**Lite lane (Phase M).** The same `CliOp`s, spooled into `~/.atelier/vm/outbox/` and applied by
the hub's poller; identity is the `ATELIER_SESSION` of the hooks-lane terminal (no satellite
process to prove it), and the hub still refuses a box not assigned to that sandbox. Writes answer
*queued (applied when the hub polls, every N s)* unless the answer arrives within 5 s.

**What "never a false success" means per command:** `emit`, `send`, `status`, `followup`,
`effect` today print "Sent/handed to Atelier" as soon as the inbox file exists; in satellite mode
they print that only once the hub acknowledged the op (`OpAnswer.state = applied`), else
*queued*. `report publish`, `pr`, `review`, `todo` writes print the hub's answer or *queued*.

---

## 5. Where the design is underspecified or conflicts with the code, and the resolution

### 5.1 How images get the Linux satellite binary (design: "same binary, Linux")
*Gap.* The hub may be Windows or macOS and cannot build a Linux binary; the design says nothing
about provenance, versions or updates. *Resolution (D5).* CI already builds `atelier` for Linux on
the glibc 2.35 floor (job `linux`, ubuntu-22.04); add an artifact step publishing
`atelier-linux-x86_64` (and `aarch64` from an ubuntu-22.04 arm runner, or 24.04-arm with a note
that its floor is 2.39). The pack names it: `vm.yaml` `satellite: { path: <file on the hub>,
sha256: <hex> }` (or `url` + `sha256`). `pack check` verifies the hash. `vm image build` uploads
it to `/opt/atelier/bin/atelier` with `put_file` (mode 0755). At claim, `SatUp::Hello.version` is
checked against `[1, SAT_PROTOCOL]`; a pool member without a hello within the claim deadline, or
out of range, gets the pack's binary uploaded and restarted through `exec` before the spawn. The
satellite protocol is versioned separately from the build, so a hub and a satellite of different
builds work together inside the range. `vm::local` runs the current executable (so on Windows and
macOS CI the "guest" is that OS: the hub uses the guest's `platform` from `HelloInfo` to choose
`PathStyle`, which also keeps the code honest about D10). S3 confirms the binary runs in the stock
image.

### 5.2 How the hub's WebSocket listener tells a satellite apart
*Gap.* Design A3 says "decided from the credential during the handshake, before any terminal list,
snapshot or document is sent", but `serve_client` sends `Ready8`, `Terminals` and
`client_connected` right after the handshake and registers every connection in `host.clients`
(C5); `Credential` has no role. *Resolution (D2, D3).* Satellite credentials are a separate store
with `sat-` ids (`remote::valid_name` accepts the form); `HostApp::remote_credential` returns
`Credential::Satellite(secret)` for them; `handshake::serve` treats it as a secret, refuses it
when not `remote`, and returns `Accepted { satellite: true }`; `serve_client` branches to
`HostApp::serve_satellite` before building a `Client`, so no `Ready8`, no terminal list, no
snapshot, no document, no broadcast ever reaches a satellite, and it never appears in
`host.clients` (so `broadcast`, `visible`, clipboard routing and the client liveness loop exclude
it naturally). The satellite must also set `CAP_SATELLITE` (a missing bit closes the link: an old
or misconfigured satellite); the bit grants nothing. Through Dev Tunnels web forwarding only paired
ids are accepted today (`serve_client` 905): `sat-` ids satisfy that. A separate file matters for
downgrades: an older service would read a `role` field in `clients.json` as unknown and accept the
satellite as a fully trusted paired client; with a separate file it refuses the unknown id.

### 5.3 A `Remote` PTY inside the `portable-pty`-based `Terminal`
*Gap.* `Terminal` is built around a `MasterPty`, a `ChildKiller`, and reader/writer/waiter
threads (C2). *Resolution (VM-21).* Keep one `Terminal` type and its emulator thread; add a remote
constructor in which the master is `None`, the `killer` is an adapter calling the link's stop, the
writer thread forwards encoded input to `RemotePty::input`, and a `RemoteFeed` replaces the reader
and the waiter (output into the same bounded channel, exit codes like the waiter, a `Ctl::Reset`
for gaps). `pid = 0` keeps every process-tree loop away (C3). `resize` calls the link besides the
emulator. `stop()` is unchanged: its Ctrl+D/Ctrl+C steps travel as input, its final kill as
`SatDown::Stop { force: true }`. The link reader never blocks on one terminal: output goes into a
per-terminal queue (8 MiB); an overflow drops the queue and asks the satellite to `Resync`.

### 5.4 The headless emulator in the satellite, and which engine
*Gap.* The design says "headless emulator (gap snapshots)" without an engine. *Resolution (D6).*
The satellite reuses `canvas_host::term::Terminal` with **alacritty**: no Zig in the Linux
satellite build, deterministic, and its query answers are the same as Ghostty's (both use
`emulator::alacritty::palette`, `ghostty.rs:201`). It owns the PTY (input, resize, signals), keeps
the screen for snapshots, and answers the program's queries locally (D7). The gap snapshot is the
terminal's `snapshot()` turned into a VT repaint by VM-22 (both engines round-trip it on the
corpus), so the hub resets its own engine (Ghostty or alacritty) by feeding bytes instead of
needing a "load from snapshot" API neither engine has. Scrollback before a gap is not restored:
the hub writes a dim marker line *output skipped while offline (N KB)* before the repaint.

### 5.5 Generations and ring sizes
*Gap.* No numbers or fencing rules. *Resolution (D8).* A generation is a `u64` the hub assigns per
terminal id at each `SatDown::Spawn`, monotonic and persisted in `launches.json` (a hub restart
never reuses one). The satellite keeps one live generation per terminal and refuses a `Spawn` for a
running one; every message names `(term, generation)` and stale ones are dropped on both sides.
Output `seq` is the absolute byte offset in the generation (replay = "from offset N"). Input `seq`
counts per (term, generation); the satellite acks the highest seq queued to the PTY writer and
ignores seqs at or below it, so the hub may resend unacked input after a reconnect (exactly once).
Sizes: ring 4 MiB per terminal (pack `link.ring_mb`, 1–16), 32 MiB per sandbox (oldest bytes of
the largest ring dropped first), unacked input 1 MiB per terminal (beyond: "VM busy: input not
sent"), outbox 1,000 ops / 16 MiB, history shipping 2 MiB/s per sandbox, file down-sync 64 MiB per
bundle, MCP 32 streams and 4 MiB in flight per sandbox, frames ≤ `MAX_FRAME` with chunks of 16 KiB
(terminal) and 1 MiB (everything else).

### 5.6 Who answers terminal queries
*Gap.* With the PTY in the guest and the emulator on the hub, a query (DA, CPR, OSC 11, kitty
`CSI ? u`) would be answered by the hub's `Sink::reply` (C4) one round trip later, not at all while
the hub is away (A2b), and a late answer can arrive after the program gave up. *Resolution (D7).*
The satellite's engine answers (same palette); the hub suppresses replies for remote terminals. The
hub still encodes keys with its own engine's modes, which follow the same output stream; after a
gap the repaint restores the modes (VM-22).

### 5.7 How `vm::local` simulates suspend/resume, egress and secrets well enough to test the rules
*Gap.* Not described. *Resolution (D19, VM-13, VM-36).*
- **Sandboxes**: folders with their own `HOME`/`ATELIER_HOME`, a seeded clone from a local bare
  repository, labels and state in a file; the satellite is the current executable.
- **Suspend/resume**: freeze the sandbox's whole process tree (`canvas_sys::process::suspend_tree`:
  SIGSTOP on Unix, process suspension on Windows) so the satellite, its PTYs and the agent stop
  exactly as a memory suspend stops them; the hub's link goes silent and is closed after 6 s;
  resume thaws them and the satellite reconnects and replays from its intact ring, which is the
  behaviour a memory snapshot gives. *Lost*: the sandbox folder removed and its tree killed behind
  the hub's back.
- **Egress**: the satellite and the agents run with `HTTP_PROXY`/`HTTPS_PROXY` pointing at
  `atelier egress-standin`, which enforces the rendered policy (default deny; host, path and method
  rules; `Transform` header injection) and logs refusals; tests call allowed and forbidden targets
  (local test servers) from inside the "guest". What a local process cannot prove is isolation
  (a process may ignore the proxy): that is the owner's egress test on Azure (§8).
- **Secrets**: `secret_set` hands values to the stand-in proxy only; tests grep the sandbox tree,
  the satellite's environment and every `Spawn` env for the value.
- **Failures**: `ATELIER_VM_TEST_FAIL` injects create/resume/exec failures, latency and 429s.

### 5.8 Token audience
*Conflict.* The design says Entra tokens for `https://management.azuredevcompute.io`; the SDK
0.1.0b4 requests `https://dynamicsessions.io/.default` (C18). *Resolution (D15).* `vm.yaml`
`auth: { audience: …, command: … }` defaulting to the SDK's audience; `pack check` prints which;
S-check on the owner's list (a 401 with the other audience is the answer).

### 5.9 Platform auto-suspend and auto-delete
*Conflict.* The platform's idle detector ignores outbound traffic (C19): a sandbox whose agent
works for half an hour without ingress or exec would be suspended under it, and auto-delete could
delete unpushed work. *Resolution (D14).* Create with `lifecycle.autoSuspendPolicy.enabled =
false` (mode `Memory` for our own `stop`) and no auto-delete (the pack may set a long safety TTL for
stopped pool members only). The canvas's `suspend_after` and the barrier own both decisions. S7
verifies that a sandbox with the policy off is not suspended while working.

### 5.10 OpenCode history is not append-only files
*Conflict.* `SatHistory { root_id, file_id, offset, bytes }` assumes appends; OpenCode keeps
SQLite (WAL) plus a live HTTP server the service polls (C10), neither mirrorable by appends.
*Resolution (VM-55).* JSONL agents (Copilot, Claude Code) are mirrored; for OpenCode the satellite
runs the OpenCode history reader in the guest and ships summaries, children, detected events and
the TUI server's state; transcripts are asked for through `ControlOp::Transcript`; resume on a new
VM uses `opencode export` / `import`. `SessionStore` gets a `Remote` root kind for it.

### 5.11 Commands are built from hub state
*Conflict.* Providers read the hub's `PATH` and environment, create hub folders and write hub files
while building commands, and format paths with `PathBuf::display()` (C9). *Resolution (VM-26,
VM-54).* `Provider::command_for(&ExecTarget, …)`: the local case is today's code; the VM case uses
the pack's program names, the guest roots the hub assigns, `PathStyle` of the guest, never touches
hub files or environment, and lists the files the command needs (instructions, MCP config, Claude
Code's private config) as bundle files for the down-sync instead.

### 5.12 `SessionStore` must work in-process for local clients
*Gap.* The design lists `Transcript`, `HasHistory`, `Fork` as `SessionStore` consumers, but those
requests run in the client's process for local clients (`LocalBackend`, C8), which cannot see
service memory. *Resolution (D17).* The store's index is a file (`<home>/vm/sessions.json`) and
mirrors are plain folders, so `LocalBackend` resolves them identically; liveness (only the service
knows it) is reported as "not live" there, which is what those requests need.

### 5.13 Older services and older clients must not run a VM box locally
*Conflict.* The env carrier alone (as `GROUP_SKILLS_ENV`) is ignored by an older service, which
would then start the agent on the hub with full access: the opposite of what the user asked.
Likewise an older client ignores `Node::placement` and sends a plain launch. *Resolution (D4).*
`AgentLaunch::agent = "vm:<kind>"` (an older service refuses "unknown agent", C13); the current
service takes the placement from the carrier, else from the document's node (`Docs` lookup by
terminal id), and refuses a launch without a carrier for a box the document places on a VM
("this box runs on a VM: update this client"). Clients show *Run on* only when the snapshot has a
`vm` view (a current service).

### 5.14 Remote terminals across service shutdown, idle exit and upgrades
*Conflict.* `Host::shutdown` stops every terminal; the client asks before restarting a service
with live terminals (C7). Design A2b says compute continues. *Resolution (VM-25).* Remote terminals
are detached on shutdown and re-adopted at start (from `launches.json`), satellites reconnect, the
screen comes back by replay (or a gap). The upgrade question counts local terminals only. Idle
exit: alive remote terminals keep the service running (it serves their links, MCP and ops), as
local ones do.

### 5.15 The design's message table misses reconciliation and hub-initiated control
*Gap.* Replay needs the hub to say what it has; claims, git checks for the barrier, satellite
upgrades, note answers and OpenCode transcripts need hub → satellite requests. *Resolution.*
`SatDown::Welcome` (per terminal generation, offset held, input acked; the root table; limits),
`SatDown::Resync`, `SatDown::Control` / `SatUp::ControlResult` (`Prepare`, `GitCheck`, `Flush`,
`Upgrade`, `ReadFile` for the viewer, `Transcript`, `Shutdown`), `SatDown::NoteResult`. All inside
the nested enums (D1), so the outer protocol grows by one variant each way.

### 5.16 Identity of relayed operations
*Gap.* "No guest-supplied identities" but the CLI's identity is an environment variable any guest
process can set. *Resolution (D10).* The satellite's endpoint finds the caller's terminal from the
peer pid (`SO_PEERCRED` / `getpeereid`; the pipe's client pid under `vm::local` on Windows) by
walking ancestors to a PTY child it spawned; the hub accepts only terminals assigned to that
sandbox. The trust boundary is the sandbox: a compromised guest can act as its own boxes, never as
another sandbox's or as you. Relayed events stay inbox-grade (`from_channel` false), so a guest
cannot trigger service-only decisions such as `pr.merged` finishing todos.

### 5.17 Time zones and clocks in the guest
*Gap.* `followup --at 09:00` uses the local zone; a guest is often UTC. *Resolution.* Computed on
the hub (§4). Hub timestamps (`now_ms`) stamp everything the hub stores; the guest's clock is only
used for its own rings and outbox order.

### 5.18 Paths shown for VM boxes
*Gap.* `SessionSummary::cwd` from a mirror is a guest path; clients call `PathInfo` on a box's
folder before launching and open folders and links on the service machine. *Resolution (VM-47).*
`Node::cwd` stays the hub folder the repo was chosen from (grouping, repo mapping, RECENT); the
launch skips the hub folder check for VM boxes; guest paths in terminals and summaries are shown
with ☁ and offer *Copy path* only (the viewer through `ControlOp::ReadFile` is a later addition).

### 5.19 The client's idle auto-stop
*Conflict.* The client stops idle agent sessions (C16), the design suspends VM sessions.
*Resolution.* The client skips boxes with `placement = Vm`; the service applies `suspend_after`.

### 5.20 Hooks for the thin lane
*Gap.* Phase M's `satellite --lite` polls "over exec", but the hook list has no exec. *Resolution
(D12).* `hooks/exec` (JSON in, JSON out); the whole hook set becomes the `vm::hooks` provider so a
pack on another platform can use the full satellite too.

### 5.21 What runs where in v1
*Gap.* Placement on passive sessions, canvas-wide agents and forks is unspecified. *Resolution
(D13).* Interactive sessions and their subsessions (same sandbox, `with`); *Open shell* boxes in a
sandbox; passive runs and canvas-wide agents stay on the hub (*Run on* hidden there); forks of a VM
session are refused with a reason (Phase 5 may add an in-guest fork through `ControlOp`).

### 5.22 Where VM state is read and changed by clients
*Gap.* The design lists RPCs, but local clients cannot reach service state through their backend
(C8). *Resolution (D11).* The view in the bus snapshot (pushed, as todos and PRs are) and
window-only bus actions for the UI (older services ignore them; refusals come back as
`ServiceMsg::Error`), the RPCs for the CLI and remote clients, answered by the service.

### 5.23 The tunnel and the hub's reachability
*Gap.* The guests need the hub's remote listener on, a Dev Tunnel hosting it and its web-forwarding
address, and the token injected by egress. *Resolution.* `pack check` and `doctor` check
`remote.listen`, `remote.tunnel`/`web_url` and the `tunnel_secret` name; the hub's URL goes into
`satellite.json`; the token never does (egress). If the hub sleeps, every VM shows *integration
offline* and the satellites queue (A2b).

### 5.24 A tenth activity view, and the ☁ glyph
*Conflict.* Alt+1…9 are taken (C15); AGENTS.md warns that some "narrow" symbols are two cells in
the Windows console (`☰`). *Resolution (D16).* A toolbar panel; a one-cell test for `☁` in the
TUI's tables and a manual look in conhost and Windows Terminal before Phase 4 merges, with `⌂` as
the fallback.

### 5.25 Credentials in pool members
*Gap.* "credentials injected only after a claim" while the satellite starts in the member.
*Resolution.* The member's satellite starts without `satellite.json` and waits (no network use); the
claim writes the file (0600) with `put_file` and the satellite connects. Pool members are never
returned to the pool after a claim (design), so a credential is never in an unclaimed member.

### 5.26 Agent CLI authentication in guests (S2)
*Open.* Whether Copilot CLI, Claude Code and OpenCode run with egress `Transform` alone or need a
token in the guest is unknown until S2. The plan keeps a `vm.yaml` `agents.<kind>.auth` field
(`egress` or `token-after-claim` with a `secrets.yaml` entry and the guest file or variable it
lands in) and treats a guest token as the design's accepted risk.

### 5.27 The data plane's shell
*Gap.* The REST API has `executeShellCommand` (non-interactive) but no interactive shell (only the
`aca` CLI has one). *Resolution.* The full lane never needs it (the satellite owns PTYs; *Open
shell* is a shell terminal through the satellite); only the thin lane uses `aca sandbox shell`
through `hooks/shell-command`.

---

## 6. Order of work and parallelism

### 6.1 Sequence

```
rename lands on main
  └─ F  foundations (lead)                                        1.5–2 d
       ├─ M  thin lane ── owner tries it on Azure (M-1..3) ──┐    5–7 d
       │                                                      │ re-estimate
       ├─ 1  manager (aca ∥ local ∥ lead: trait/store/manager/launcher)   7–9 d
       │     └─ 0 kit: s2, s3 ───────────── owner runs ───────┤
       ├─ 2  terminals (satellite+repaint ∥ SessionStore ∥ lead: Remote PTY, link, Host, ExecTarget)  10–13 d
       │     └─ 0 kit: s1, s4, s7 ───────── owner runs ───────┤
       ├─ 3  integration (Io+CLI ∥ egress+secrets ∥ MCP ∥ lead: ops relay, sync, profile)  10–13 d
       │     └─ 0 kit: s5 ──────────────── owner runs ───────┘
       ├─ 4  UX (canvas-core worker ∥ lifecycle worker ∥ lead: parity, self-test)   5–7 d
       └─ 5  hardening (Claude ∥ OpenCode ∥ lead: --real kit, soak, resume on new VM)   7–9 d
```

Phases are merged into `main` one at a time after their verification (VM-SESSIONS.md *As built*
updated each time). M can be merged before Phase 1 starts; Phase 1's `vm::hooks` absorbs M's
hooks runner.

### 6.2 Hot files: owned by the lead, edited one change at a time

`crates/canvas-common/src/protocol.rs`, `canvas-common/src/handshake.rs`,
`canvas-model/src/{doc,rpc,bus,launch}.rs`, `canvas-host/src/{term,server}.rs`,
`canvas-agents/src/service/{mod,passive}.rs`, `canvas-agents/src/{index,backend}.rs`,
`canvas-core/src/{app,actions,ui}.rs`, `src/main.rs`, `src/parity.rs`, `src/selftest.rs`,
`.github/workflows/ci.yml`, README.md and the topic docs. Workers expose functions in their own
files; the lead wires them (one or two lines each) after merging the worker's branch.

### 6.3 Worker packages (disjoint files; each can be given to a parallel worker)

| Package | Phase | Owns (new unless noted) | Depends on | Hand-off |
|---|---|---|---|---|
| P-pack | M | `canvas-agents/src/vm/pack*.rs`, `docs/examples/vm-pack/**` | F | `Pack::load`, `check()` |
| P-hooks | M | `canvas-agents/src/vm/hooks.rs`, `src/vm_hook_standin.rs` | F, P-pack types | `Hooks` runner, stand-in |
| P-ui-m | M | `canvas-core/src/{dialogs,ui_modals,modals}.rs` (NewSession *Run on* only), `tasks/flows.rs`, `tasks/detail.rs` | F | snapshots |
| P-aca | 1 | `canvas-agents/src/vm/aca/**` | VM-11 trait | `AcaProvider`, HTTP stand-in tests |
| P-local | 1 | `canvas-agents/src/vm/local.rs`, `canvas-sys/src/process/*` (suspend/resume only) | VM-11 trait | `LocalProvider` |
| P-sat | 2 | `canvas-agents/src/vm/satellite/**`, `canvas-host/src/emulator/replay.rs` | F (sat enums), `Hooks::raw` (lead adds first, 10 lines) | `satellite::run`, `replay::repaint` |
| P-store | 2 | `canvas-agents/src/vm/session_store.rs` (+ the lead applies its small diffs to `index.rs`, `backend.rs`, `passive.rs`) | F | `SessionStore`, `Index::with_liveness` |
| P-cli | 3 | `canvas-agents/src/cli_io.rs`, the `*_cli.rs` modules, `reports.rs` publish path, `vm/satellite/client.rs` | F (`CliOp`) | `Io`, `client::dispatch` |
| P-egress | 3 | `canvas-agents/src/vm/{egress,secrets}.rs`, `src/egress_standin.rs` | P-local | rendered policies, stand-in |
| P-mcp | 3 | `canvas-agents/src/vm/mcp/**`, `vm/satellite/mcp_proxy.rs`, `src/mcp_standin.rs` | P-sat link API | gateway, stand-in |
| P-ux | 4 | `canvas-core/src/{vms.rs, ui_side.rs (VM section), ui_canvas.rs, view.rs, tasks/forms.rs}`, `canvas-gui/src/canvas.rs` (marker) | F views | snapshots |
| P-life | 4 | `canvas-agents/src/vm/quiesce.rs` (+ manager hooks via the lead) | Phases 1–2 | barrier |
| P-claude / P-opencode | 5 | their provider and history files, `vm/satellite/opencode.rs` | Phase 2 | probes |

Subagents may not be able to spawn further subagents: the lead works sequentially through the
hot-file items and asks the manager to run 2–3 of the packages above in parallel per phase (the
"∥" in §6.1). A worker gets: this plan's rows for its package, the F types, the files it owns,
the files it must not touch, and the verification commands for its crate.

### 6.4 Build and verification on this machine (≈12 GB free, shared)

- One worktree `../atelier-vm` (branch `vm`), one shared `CARGO_TARGET_DIR` for the lead and every
  worker (cargo serializes builds on its lock; artifacts are reused), `CARGO_BUILD_JOBS` limited,
  builds at low priority, outputs to files. A second target dir does not fit.
- No Zig here: local builds and tests use `--no-default-features` (alacritty); the Ghostty side of
  the repaint round trip runs in CI. Windows is checked locally with `cargo xwin clippy --target
  x86_64-pc-windows-msvc --workspace --exclude ghostty-vt-sys --no-default-features --all-targets
  -- -D warnings` (as PORT-STATUS does) and fully in CI.
- Per change: clippy and the crate's tests; per phase: the full `canvas-verify` list on the
  platforms CI covers, plus `selftest vm` (no windows, can run beside builds), and the GUI suites one
  at a time.

---

## 7. Risks and estimates

### 7.1 Estimates (agent-days, re-checked against the code)

| Phase | Design | This plan | Why it differs |
|---|---|---|---|
| F foundations | — | 1.5–2 | not in the design: the shared types first, so workers can start |
| M thin lane | 4–6 | 5–7 | pack loader done properly once (reused), fail-closed launches (§5.13), the lite op executor is Phase 3's first half |
| 0 spikes | 3–4 | 2–3 (ours) + owner runs | runs are the owner's; we build the kit and local analogs |
| 1 manager | 5–6 | 7–9 | `vm::aca` against REST with token caching and an HTTP stand-in, `vm::local` with freezing on three OSes, the store and the satellite credential store |
| 2 terminals | 7–9 | 10–13 | the Remote PTY plus the VT repaint (round trip on the corpus for both engines), re-adoption across service restarts (§5.14), file-backed `SessionStore` (§5.12), target-aware commands (§5.11), a TLS/proxy dialer |
| 3 integration | 7–9 | 10–13 | the `Io` seam through six CLI modules, the egress engine and stand-in proxy, MCP Streamable HTTP termination in the satellite |
| 4 UX | 4–5 | 5–7 | six parity rows, both clients, remote mode, the barrier's questions |
| 5 hardening | 5–7 | 7–9 | OpenCode cannot be mirrored (§5.10), soak harness |
| **Total** | 35–45 after M (39–51) | **47.5–63** | |

With 2–3 parallel workers on the packages of §6.3, the lead's critical path is about 30–40
days. Phase M's Azure numbers and the owner's spike results re-estimate Phases 2–5 (the design's
own rule).

### 7.2 Risks

| Risk | Likelihood / impact | Mitigation |
|---|---|---|
| The guest cannot reach the hub through the tunnel's web forwarding with an egress-injected header (TLS interception, proxy env, upgrade handling) | medium / high | S1 early (Phase 2 kit); the dialer supports direct, `HTTP_PROXY` absolute-form, `HTTPS_PROXY` CONNECT, an extra CA; fallbacks from the design (a port restricted to one email; streaming exec) carry the same protocol |
| Preview API churn (SDK 0.1.0b4; audience; field names) | high / medium | one provider module, versions in the pack, golden request bodies in tests, `doctor` check; the owner's `vm spike` runs after each SDK release |
| Platform idle suspend freezing working agents | medium / high | auto-suspend off at create (D14); S7 verifies |
| The rename collides with this work (every file touched) | certain / medium | start after it lands; re-check wire indices and env names; no edits before |
| Hot-file contention (`service/mod.rs`, `server.rs`, `term.rs`) | high / medium | lead-owned, small wiring diffs, workers in new files (§6.2) |
| Gap reset fidelity (engines differ, scrollback lost) | medium / low | corpus round trip for both engines; a visible gap marker; ring sized for typical outages |
| Agent CLI auth needs a token in the guest | medium / medium | accepted risk in the design; inject after claim, scoped, deny-default egress (S2) |
| History mirroring errors (rotation, truncation, huge histories) | medium / medium | file identity in every append, offsets continuous or rejected, rate limits, the Copilot index's own 2 MiB tail bound |
| MCP protocol surface (sessions, SSE, server-initiated requests) and hub process count | medium / medium | per-session lazy processes, idle stop, pinned protocol versions, shared mode only after its tests (VM-56) |
| Windows hub building Linux paths | medium / low | `ExecTarget` with `PathStyle`, model_launch tests on every OS for a Windows-style hub |
| Security: an untrusted guest floods or traverses | medium / high | per-link limits, name checks on both ends, terminal-in-sandbox and generation fencing, negative tests (traversal, cross-sandbox, flood), inbox-grade events |
| Pack values leaking into logs or the repository | low / high | redaction in `Debug`/`Display`, never logging hook output beyond its first line and size, tests that grep logs for planted values; `pack check` warns on a public work tree |
| Disk and CI time (≈12 GB locally; Windows CI already long) | high / medium | shared target dir; `selftest vm` ≈ 60–90 s; parity rows in `late_rows` with their own small canvas |
| A downgraded hub with VM boxes | low / high | fail-closed launches (§5.13), separate satellite credentials (§5.2) |
| Upgrading the satellite in running sandboxes | medium / low | satellite protocol range, upgrade only between sessions or through `ControlOp::Upgrade` after a flush; pool recycling brings new images |

---

## 8. The owner's checklist and the example pack

`docs/VM-OWNER-CHECKLIST.md` (Phase 0) lists, in order, with the expected output of each:

1. Prerequisites: an Entra account with *Container Apps SandboxGroup Data Owner* on the group's
   scope; `az login`; a sandbox group in a region; a Dev Tunnel hosting the hub's remote port with
   web forwarding; a group secret holding a tunnel connect token (refreshed by `secrets.yaml`);
   S6 policy acknowledged in `vm.yaml` (`policy_ack`).
2. Copy `docs/examples/vm-pack/` outside any repository, fill it (`contoso`/`example.com` →
   real values), `atelier vm pack check`.
3. Phase M: M-1 the hook scripts (claim, shell-command, exec, suspend, resume, delete, image) on a
   real group; M-2 an agent CLI authenticated in a guest; M-3 first-prompt latency through hooks.
4. Spikes as they land: `atelier vm spike s2|s3` (Phase 1), `s1|s4|s7` (Phase 2), `s5` (Phase 3).
5. `atelier selftest vm --real` once per agent (Phase 5), and the egress isolation test (a guest
   process that ignores proxy settings is still refused).
6. Report: the JSON outputs (they contain no pack values) attached to the phase's PR.

Example pack (`docs/examples/vm-pack/`, values are placeholders only):

```
vm.yaml            pack_version: 1; provider: aca; subscription/resource group/sandbox group/region
                   (contoso placeholders); api_version; auth: { audience, command }; sizes (S, M, L
                   mapped to cpu/memory/disk); pool: { min: {repo: n}, max_age: 12h }; images:
                   { schedule: daily 02:00, keep: 3 }; budget: { max_running, max_cores, daily_soft };
                   suspend_after: 30m; tunnel: { url, secret }; git: { name, email };
                   satellite: { path, sha256 }; link: { ring_mb: 4 }; policy_ack: ""
repos.yaml         contoso-app: { url: https://example.com/contoso/app.git, default_branch: main,
                   clone: { mode: blobless }, image: { base: ubuntu, disk: 20Gi },
                   egress: forge, folders: ["**/contoso-app"] }
egress/forge.yaml  default Deny; allow example.com git over HTTPS for {{repo.path}};
                   Transform: Authorization from {{secret:forge-token}} on example.com/api;
                   the tunnel host with X-Tunnel-Authorization from {{secret:tunnel-token}}
secrets.yaml       forge-token: { command: "example-cli auth token", key: value, every: 50m }
mcp-policy.yaml    servers: { forge: { mode: per_session, rules: [allow list_pull_requests;
                   allow create_pull_request when draft == true and head == {{box.branch}};
                   deny vote; deny update_pull_request] } }
profile.yaml       include: [~/.copilot/skills/*, ~/.claude/skills/*]; exclude: [**/*token*, **/.env]
paths.yaml         C:\notes\contoso → /home/agent/notes (rw-notes)
hooks/             claim, shell-command, exec, suspend, resume, delete, image (bash and .ps1, aca CLI)
```

---

## Appendix A: Azure Container Apps Sandboxes data plane (from the Python SDK 0.1.0b4)

Base `https://management.<region>.azuredevcompute.io`, `?api-version=2026-02-01-preview`, bearer
token (SDK scope `https://dynamicsessions.io/.default`, §5.8). Group path `G =
/subscriptions/{sub}/resourceGroups/{rg}/sandboxGroups/{group}`; sandbox path `S = G/sandboxes/{id}`.
The ARM control plane (`management.azure.com`, `/providers/Microsoft.App/sandboxGroups`) creates
groups only: the pack assumes the group exists.

| Operation | Call | Body / notes |
|---|---|---|
| create | `PUT G/sandboxes` | `{ sourcesRef: { diskImage: { id } \| { name, isPublic: true } } \| { snapshot: { id } }, resources: { cpu: "2000m", memory: "4096Mi", disk }, lifecycle: { autoSuspendPolicy: { enabled: false, interval, mode: "Memory" } }, labels, environment (not for secrets), egressPolicy, volumes, ports, entrypoint, cmd, skipEgressProxy }` → sandbox `{ id, state, labels, … }` |
| list | `GET G/sandboxes?labels=k=v,k2=v2` | `{ value: [...], nextLink }` or a list |
| get / delete | `GET S`, `DELETE S` | states `Creating, Running, Stopping, Stopped, Suspended, Resuming, Deleting, Failed`; `stateDetails.stoppedReason` |
| suspend / resume | `POST S/stop`, `POST S/resume` | 200/202/204; poll `GET S` |
| exec | `POST S/executeShellCommand` | `{ command, workingDirectory }` → `{ exitCode, stdout, stderr }` |
| files | `GET S/files?path=`, `PUT S/files?path=` (bytes), `DELETE S/files?path=`, `GET S/files/list?path=`, `GET S/files/stat?path=`, `POST S/files/mkdir` | |
| egress | `POST S/egresspolicy`, `GET S/egress-decisions` | `{ defaultAction: Allow\|Deny, hostRules: [{ pattern, action }], rules: [{ name, match: { host, path, methods }, action: { type: Allow\|Deny\|Transform\|Rewrite, headers: [{ operation: Set\|Insert\|Remove, name, value \| valueRef: { secretRef: { secretId, secretKey, format } \| managedIdentityRef } }] } }], trafficInspection }`; setting it resumes a stopped sandbox |
| lifecycle | `POST S/lifecycle` | `{ autoSuspendPolicy: { enabled, interval, mode }, autoDeletePolicy: { enabled, deleteIntervalInSeconds } }` |
| snapshot / commit | `POST S/snapshot`, `POST S/commit` | `{ labels: { name } }`; commit answers `{ diskImage: {…} }` |
| stats | `GET S/stats` | |
| ports | `POST S/ports/add`, `POST S/ports/remove`, `PUT S/ports` | not used (the link is outbound) |
| disk images | `PUT G/diskimages`, `GET G/diskimages`, `GET/DELETE G/diskimages/{id}`, `GET G/diskimages/public` | `{ image: { base, entrypoint, cmd }, labels, registryCredentials, managedIdentityResourceId }` |
| secrets | `PUT G/secrets/{id}`, `DELETE G/secrets/{id}`, `GET G/secrets`, `GET G/secrets/{id}/keys` | `{ values: { key: value } }` (never logged; `peek` not used) |

Sizes from the overview: XS 0.25/0.5 GiB/5 GiB … XL 4/8 GiB/80 GiB … 3XL 16/32 GiB/320 GiB;
default quota 300 concurrent cores per region per subscription (design §2).

## Appendix B: what `vm::local` proves in CI (Windows, macOS, Linux)

| Proven on all three | Not provable locally (owner, §8) |
|---|---|
| the satellite role before any client message; refusals on the pipe and after revocation | the Dev Tunnel web forwarding path and egress header injection (S1) |
| launch intents surviving a service kill at every step; re-adoption after restart | real boot/resume latency and p50/p95 (S4; local is a lower bound) |
| replay inside the ring, gap reset beyond it, exactly-once input, fencing | memory-suspend semantics of the platform (S7) |
| history mirroring, status, transcript, subsessions from the guest | agent CLI auth in a guest (S2) |
| CLI ops (every §4 row), offline queueing, idempotent replays | the stock image running the Linux build (S3; Linux CI runs `--self-check`) |
| file sync with traversal and cross-sandbox negatives; note conflicts | egress isolation against a process ignoring proxies |
| egress rule evaluation and header injection with secrets never in the guest | a real MCP server's behaviour (S5; the stand-in covers the protocol) |
| MCP policy: filtered lists, refused vote/update, allowed draft-create for the box's branch, one process per session | |
| pool, images, budgets, suspend_after and the quiescence barrier with an injected clock | |
| the UI (parity rows 57–62, snapshots) and remote mode over loopback | |
