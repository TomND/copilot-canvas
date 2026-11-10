# VM sessions: status

The working log of [VM-SESSIONS.md](VM-SESSIONS.md) (the design) and
[VM-SESSIONS-PLAN.md](VM-SESSIONS-PLAN.md) (the approved plan). Read this first to resume after an
interruption, then `git log` on `vm-sessions`.

- Branch `vm-sessions` in the worktree `../atelier-vm` (made from `origin/main` at 3f6551e, after
  the rename to Atelier). `origin/main` is merged in regularly (merge, never rebase).
- At the end of each phase, once CI on `vm-sessions` is green on every job, `vm-sessions` is merged
  into `main` in the main checkout (fetch, merge, push only) and work continues on `vm-sessions`.
- **VM sessions are optional; local stays the default.** Without a pack nothing changes and no VM
  UI shows; with one, *Run on* defaults to *This machine* and `Placement` to `Local` everywhere.
- No real cloud: `vm::aca` is tested against a local HTTP stand-in only; everything else runs on
  `vm::local`. Checks that need real sandboxes are the owner's (below).

## Decisions (approved 2026-11-09)

The ten of VM-SESSIONS-PLAN.md (nested `Sat` messages at index 29 with `CAP_SATELLITE = 1 << 6`;
`<home>/vm/satellites.json` with `sat-` ids decided before `Ready8`; `vm:<kind>` launches; the Linux
release build as the satellite, pinned by SHA-256 in the pack; queries answered in the guest with
alacritty; typing refused while the link is down, buffered 4 KiB / 30 s while resuming; platform
auto-suspend and auto-delete off; the token audience a pack setting; v1 scope without passive runs,
canvas-wide agents or forks on VMs; ☁ VMs as a toolbar panel with a one-cell glyph check), plus the
owner's note that VM sessions are optional and local stays the default.

## Phases

### F: foundations (done 2026-11-09, c90c165)
- [x] VM-F1 satellite protocol (`canvas_common::sat`), `CAP_SATELLITE`, `ClientMsg::Sat` / `ServiceMsg::Sat` at 29, shaping
- [x] VM-F2 `Credential::Satellite`, `Accepted::satellite`, refused on the pipe
- [x] VM-F3 model types (`Placement`, `VmPlacement`, `canvas_model::vm`, `Snapshot::vm`, `AgentPolicy::start_on_vm`, launch carrier and `vm:` agents, `Vm*` requests)
- [x] VM-F4 `settings.json` `vm.pack`, `ATELIER_VM_PACK`
- [x] VM-F5 `canvas_agents::vm` (the module; `vm::service_request`)
- [x] Exit: clippy (macOS), `cargo xwin clippy`, the touched tests (CI runs with Phase M's: F was not merged on its own)

### M: thin lane (done 2026-11-09; CI run 42742983720 green on every job; merged into main at d35d56e)
- [x] VM-M1 pack loader, `atelier vm pack check`, example pack (`docs/examples/vm-pack/`)
- [x] VM-M2 hooks runner and `atelier vm-hook-standin`
- [x] VM-M3 placement in launches (fail closed)
- [x] VM-M4 `atelier vm attach`
- [x] VM-M5 `atelier satellite --lite`, guest outbox
- [x] VM-M6 hub poller, the `CliOp` executor (first verbs)
- [x] VM-M7 status from mirrors (liveness overlay)
- [x] VM-M8 minimal UI (*Run on*, ☁ marker, *Start on VM…*), only with a pack
- [x] VM-M9 docs
- [x] Exit: `selftest vm` (the hooks lane) on Windows, macOS, Linux; no-pack snapshots and self-test unchanged; merged into main

### 0: spike kits
- [x] `atelier satellite --self-check` (a PTY with alacritty, the data folder) and `atelier vm spike s2|s3|s4`
      through the pack's provider (hooks, local or aca; JSON, no pack values; sandboxes deleted unless `--keep`;
      S3 uploads `satellite.path` first); run on local sandboxes through `hooks` and `local` by `selftest vm` in CI
- [ ] `vm spike s1` (Phase 2: the satellite link), `s5` (Phase 3: the MCP gateway), `s7` (Phase 2/5: faults)
- [x] S6: `policy_ack` in `vm.yaml` (`pack check` warns while it is empty); the owner's checklist below

### 1: manager (done 2026-11-10; CI run 42771716398 green on every job; merged into main at 36e0bcf with Phase 2's core)
- [x] VM-11 provider trait (`vm::provider`); the pack's hooks as a provider (`vm::hooks::HooksProvider`)
- [x] VM-12 `vm::aca` (P-aca, `vm-aca` 6ceef0d..9d4a8e8) and VM-13 `vm::local` with `canvas_sys::process::
      {suspend_tree, resume_tree}` (P-local, `vm-local` 6ceef0d..ec3079d): merged into `vm-sessions` 2026-11-10
- [x] VM-14 stores and satellite credentials (`vm::store`) · VM-15 manager (`vm::manager`: budgets, image schedule
      and keep-N, pool plan and tick, image builds) · VM-16 launcher (`vm::launcher`: steps, fence, rollback,
      reconciliation) · VM-17 claim (lease or create, credential in the guest, `satellite --prepare`) · VM-18 the
      view, `VmAction`, `VmImageBuild`, `atelier vm …` · VM-19 `has_work`, doctor
- [x] the secrets refresher (`vm::secrets`, in the design's Phase 1; the plan had it in Phase 3): each
      `secrets.yaml` command's output set on the provider on its interval, never logged or stored
- [x] wiring: `open_provider(home, pack)` for `local` (`local.root`, else `<home>/vm/local`) and `aca`
      (`Aca::from_pack`); the provider contract settled (departures below); `selftest vm`'s pool lane on
      `provider: local` (an image built in a builder, a warm VM leased, its branch prepared, the agent in its
      terminal, the pool refilled, the VM deleted); `vm spike` through a provider; `rustls` (ring) and
      `rustls-platform-verifier` in canvas-agents on every OS with `vm::tls` (the satellite's `wss://` config)
- [x] Exit: `selftest vm` with the pool lane on three OSes, both providers wired, `vm spike` through a provider;
      the owner's checks on real sandboxes stay below

### 2: terminals
- [x] VM-21 remote PTY (`Terminal::spawn_remote`, `RemotePty`, `RemoteFeed`; started early, unit-tested)
- [x] VM-22 VT repaint (`emulator::replay::repaint`; round trip on the alacritty engine; the corpus round trip
      for both engines comes with the satellite)
- [x] VM-23 the satellite: `vm::satellite::{dial, terms, ring}` (tcp/ws/wss with rustls, platform roots and the
      pack's CA, `CONNECT` proxies; PTYs with alacritty, a 4 MiB ring, replay or repaint; generations; input once;
      reconnect 0.5/1/1.5 s; heartbeats; ends on a proven revocation), `atelier satellite [--start]`; history and
      relayed commands through the drain as a control (departure below). Still to come: the endpoint for the guest
      CLI (Phase 3), `proc`, `stats`, a per-sandbox ring cap
- [x] VM-24 hub link (`vm::link`): one link per sandbox, Hello first and its own terminals only, output in order,
      input numbered and resent, controls, heartbeats and the 6 s silence rule; flood limits still to come
- [x] VM-25 launch, shutdown, re-adoption: `Resolved::remote`, `Host::adopt_remote`, `Intent::terminal`, remote
      terminals let go at shutdown and adopted at start; the launcher starts the satellite (`Provider::start_satellite`)
- [ ] VM-26 `ExecTarget`: the guest's command line comes from `launch::guest_command` (no hub paths); the guest roots
      (plugin, skills, instructions, notes, MCP) come with Phase 3's file sync
- [x] VM-27 `SessionStore` for Copilot (`vm::session_store`): `HasHistory`, `Transcript`, `Fork` refused for mirrors
- [ ] VM-28 guest subsessions · VM-29 the rest (the probe's drops during output and beyond the ring; input while
      resuming and the mailbox's wait are done and unit-tested) · VM-2A docs (BACKEND, REMOTE, README, VM-SESSIONS
      written; AGENT-PROVIDERS and DEVELOPMENT's code map with VM-26)
- [ ] Exit: `selftest vm` (replay, gap, freeze, restart) on three OSes

### 3: integration
- [ ] VM-31 `Io` seam · VM-32 CLI satellite mode · VM-33 op relay · VM-34 file sync · VM-35 profile and path map · VM-36 egress and secrets · VM-37 MCP gateway · VM-38 guest MCP config · VM-39 docs and skills
- [ ] VM-36 egress (worker P-egress, `vm-egress`) and VM-37 MCP gateway (worker P-mcp, `vm-mcp`): prompts in
      VM-WORKERS.md; their interfaces landed (`vm/egress/mod.rs` denies everything, `vm/mcp/mod.rs` refuses every
      stream, `vm/satellite/mcp_proxy.rs` serves nothing; the link and the satellite already call them)
- [ ] Exit: every row of the CLI inventory tested; offline writes say *queued*

### 4: UX
- [ ] VM-41 *Run on* · VM-42 box states · VM-43 details · VM-44 Tasks · VM-45 ☁ VMs panel · VM-46 lifecycle and barrier · VM-47 client exclusions · VM-48 parity rows 57–62 and the self-test step · VM-49 docs
- [ ] Exit: parity rows pass, snapshots read, self-test step in both clients

### 5: hardening
- [ ] VM-51 `selftest vm --real` · VM-52 soak · VM-53 resume on a new VM · VM-54 Claude Code · VM-55 OpenCode · VM-56 shared MCP · VM-57 docs
- [ ] Exit: as in the plan

## Owner-only items (need real sandboxes; never run in CI)

Prerequisites: an Entra account with *Container Apps SandboxGroup Data Owner*, a sandbox group, a
Dev Tunnel hosting the hub's remote port with web forwarding, a pack copied from
`docs/examples/vm-pack/` outside any repository and filled in, `atelier vm pack check` clean.

- [ ] M-1 the example pack's aca hook scripts against a real group
- [ ] M-2 an agent CLI authenticated in a guest
- [ ] M-3 first-prompt latency through hooks
- [ ] S1 satellite through the tunnel's web forwarding, role before any client message (`atelier vm spike s1`)
- [ ] S2 agent CLI auth in a guest (`vm spike s2 --agent …`)
- [ ] S3 the Linux build in the stock image (`vm spike s3`)
- [ ] S4 first prompt p50/p95 from a warm pool (`vm spike s4 --runs 20`)
- [ ] S5 MCP policy and per-session processes with a real server (`vm spike s5 --server NAME`)
- [ ] S6 the data-handling policy for the pack (`policy_ack` in `vm.yaml`)
- [ ] S7 failure injection on the platform (`vm spike s7`), incl. auto-suspend off while working
- [ ] the token audience (`auth.audience`: a 401 with the other value answers it)
- [ ] `vm::aca` against a real group: the answers' shapes (image readiness, egress decisions, raw-file GET), the
      files API and `/opt/atelier/bin` in the stock image (`vm spike s3` uploads there), token expiry
- [ ] egress isolation: a guest process that ignores proxy settings is still refused
- [ ] `atelier selftest vm --real` once per agent (Phase 5)

## Departures from the plan

- **Relayed commands run as a subprocess** (Phase M; replaces most of Phase 3's VM-31 `Io` seam): the hub runs
  `atelier <words>` with the box's identity (`ATELIER_SESSION`, `ATELIER_HOME`), the files the command reads
  carried in the operation and written to a folder of their own, its outputs read back. The command then follows
  exactly the rules a local session's command follows, with no change to the six CLI modules. Phase 3 keeps the
  satellite endpoint (identity from the process tree) and adds nothing to the CLI modules unless a verb needs it.
- **Launch intents are one file per box** (`<home>/vm/launches/<box>.json`) instead of one `launches.json`.
- **`Placement::Vm` holds a `Box<VmPlacement>`** (clippy's large-variant rule on `Request`, whose `Group` carries
  launch defaults).
- **The new-session dialog's VM rows**: *Run on*, then *Repository* with *size* on one row and *Branch*; a VM
  session gets no worktree and no fork choice. Copilot CLI only until Phase 5.
- **The provider contract** (settled at the workers' merge): `put_file(id, path, bytes, mode)` (the plan's VM-11 had
  the mode; the first trait dropped it); guest paths are relative to the guest user's home or absolute (`vm::local`:
  absolute only inside the sandbox's `home/`); `get_file` over `max` is an error everywhere; `exec` of `atelier`
  runs the satellite (`aca` puts `/opt/atelier/bin` first on `PATH`). `vm::aca` writes a file as a staging upload
  plus one `install -D -m MODE` (the files API sets no mode and may not make folders) and asks a sandbox's `$HOME`
  once for relative reads. `vm::local`'s disk is `home/` only: the workspace moved from `<sandbox>/work` to
  `<sandbox>/home/work` (where the guest's `~/work/<repo>` is), so images carry the clone.
- **`vm attach` on `provider: local`** runs the agent in the sandbox directly (`Local::command`, tracked so suspend
  and delete reach it) until Phase 2's satellite link carries every provider's terminals; `aca` boxes reach
  *Running* but have no terminal before Phase 2.
- **The spikes go through `open_provider`** (the plan had them on `vm::aca`; Phase 0 had them on the hooks).
- **The satellite's TLS uses `rustls-platform-verifier`** (already in the lock through reqwest) for the platform's
  roots, with the pack's CA as extra roots.
- **Every provider but `hooks` uses the satellite** for a box's terminal (`hooks` keeps `vm attach` and the hooks'
  shell); `vm::local`'s boxes too, so the pool lane tests the real path.
- **History and relayed commands go through the thin lane's drain sent over the link** (`SatDown::Control` /
  `SatUp::ControlResult`) instead of `SatUp::History` / `SatUp::Op` pushes: the same mirror and execution code,
  polled every `poll_seconds` without a provider command. Pushing appends as they happen can replace it later.
- **The satellite is started by the launcher** (`Provider::start_satellite`: `atelier satellite --start` in the
  guest; `vm::local` starts it as a tracked process outside the service's Windows job) after the branch is
  prepared, instead of at sandbox creation; `CONNECT` is used through proxies for `ws://` too.
- **The satellite honours `ATELIER_<AGENT>`** for an agent's program, as the hub does (the self-tests' stand-ins).
- **The remote listener** binds with `SO_REUSEADDR` on Unix and retries for 15 s at start (found by the restart step).
- **No note line marks a gap** (VM-29 had one): a guest's ConPTY (Windows) draws by absolute positions, so nothing
  of the hub's may be drawn into a program's screen once it runs; a gap is logged, and a program's first output
  starts on a blank screen (the launch's step lines are cleared).
- **`vm::local` on Linux starts its satellite in a systemd user scope of its own**, out of the service's unit, whose
  stop ends its cgroup (a VM's satellite is not the hub's process at all).

## Log

- 2026-11-09: worktree and branch made from `origin/main` 3f6551e; the plan copied into
  docs/VM-SESSIONS-PLAN.md with the approval; this file.
- 2026-11-09: Phase F (c90c165): `canvas_common::sat`, `CAP_SATELLITE`, `Sat` at 29, `Credential::Satellite`,
  `HostApp::serve_satellite`, `canvas_model::vm`, placements, `Snapshot::vm`, `Vm*` requests, `vm.pack`.
- 2026-11-09: Phase M: the pack (`vm pack check`, `config vm.pack`, the example pack), hooks, `vm::local` and
  `vm-hook-standin`, intents and `vm attach`, the guest relay and `satellite --lite`, the hub's drains, mirrors and
  relayed operations, the mirror index, *Run on* / ☁ / *Start on VM…* with a pack only (3 new snapshots, every
  other unchanged), `selftest vm` (PASS in 13 s on macOS; added to CI on all three OSes), the backend contract's
  VM requests, docs.
- 2026-11-09: Phase 0 (partial, by design): the spikes S2–S4 kit and `satellite --self-check`, checked by
  `selftest vm` on local sandboxes; S1, S5, S7 land with the phases that make them possible.
- 2026-11-09: CI green on 93556b4 (run 42742983720: Windows test with `selftest vm`, parity, goldens; macOS;
  Linux ×3 and linux-gui; iOS; packages). Merged into main (d35d56e, main had not moved). Phase 1 started: the
  provider trait; worker prompts for `vm::aca` and `vm::local` in docs/VM-WORKERS.md.
- 2026-11-10: Phase 1 (lead's part): stores, credentials, manager, launcher, hooks provider, guest commands, the
  view and actions, `atelier vm …`, doctor; `selftest vm` adds the credential in the guest, a budget queue, suspend
  and resume, delete with the credential revoked (PASS in 20 s on macOS). Waiting for the workers' `vm::aca` and
  `vm::local` to wire them into `open_provider` and the pool lane's self-test.
- 2026-11-10: the secrets refresher; Phase 2 started early on files no worker owns: the VT repaint and remote
  terminals in `canvas-host` (both unit-tested). The satellite (VM-23) waits for the P-aca hand-off: its
  `wss://` dialer needs `rustls` in `crates/canvas-agents/Cargo.toml`, which that worker owns.
- 2026-11-10: CI run 42749108451 (2644eaf) failed only in `selftest vm`: arm64 Linux checked the revocation before
  the launcher had revoked (the probe now waits for it), macOS saw "2 of 1 running" (budgets now count the
  launches' running VMs, not sandbox records; the probe checks no sandbox leaked). Fixed in 88defa0 (3 local passes);
  its CI is queued behind the workers' branches. Main's CI for the d35d56e merge is still running.
- 2026-11-10: the workers handed off. `vm-aca` and `vm-local` merged into `vm-sessions` (one conflict: the module
  list). The provider contract settled (departures), `open_provider` wired for both, `vm attach` and the spikes
  through providers, the pool lane in `selftest vm` (both lanes PASS in 34–35 s on macOS, three runs), `vm::tls`
  with rustls on every OS. 74 `vm::` tests, 289 canvas-agents lib tests, 18 canvas-sys tests; clippy clean on
  macOS and `cargo xwin` (Windows). Windows CI runs `vm::local`'s Windows suspend for the first time.
- Owner items added by `vm::aca` (guesses from Appendix A, checked only on real sandboxes): image readiness, the
  egress-decisions and raw-file GET shapes, whether the files API makes folders (writes go through `install -D`
  either way), whether `/opt/atelier/bin` is writable by the files API in the stock image (S3), token expiry from
  `expires_on` or the token's `exp`; no re-fetch on a 401 yet.
- 2026-11-10: Phase 2's core on `vm-sessions` (not pushed while CI ran for 7433d60): the satellite and the hub link
  (tcp/ws/wss, rustls with a test CA valid 2019–2099 in `vm/satellite/testdata`), remote terminals launched,
  adopted after a restart, kept through a frozen VM; drains over the link; `SessionStore` for Copilot. `selftest
  vm` PASS in 50 s on macOS (both lanes), three runs. Found and fixed: the probe's `vm …` commands started a stray
  `default` service in the probe's home (with the real Copilot folder as its sessions folder) that ran a second VM
  manager: they now name the probe's instance; the probe ends every sandbox's processes, pass or fail.
- 2026-11-10: CI on the Phase 2 work found three things, all fixed: Linux runners stopped the guest's programs at a
  restart of the service (its systemd unit ended its cgroup: `vm::local` now starts the satellite in a scope of
  its own); on Windows the guest's ConPTY drew over the launch's step lines (a program now starts on a blank screen
  and nothing of the hub's is drawn into it); the new repaint round trip in `corpus check` met Ghostty dropping a
  keycap's VS16 (between engines the hub's own drawing of the recording is accepted). The earlier Windows hang was
  the probe's stray service holding its output (fixed by naming the probe's instance; every probe command is now
  bounded, the step has 15 minutes). In-process tests now cover the hub link (Hello, ordering, input, resume
  buffer, the mailbox's wait, offline stops) and drops end to end (exact replay within the ring, repaint beyond).
  Pending for Phase 2: VM-26 guest roots (with Phase 3's sync), VM-28 subsessions, the link's flood limits,
  `proc` and `stats`, DEVELOPMENT and AGENT-PROVIDERS docs.
- 2026-11-10: Phase 1 merged into main at 36e0bcf (CI run 42771716398 green on all 17 jobs), together with Phase 2's
  core (the satellite link; gated by the pack like everything else): the run on the Phase-1-only commit hung on
  Windows (the probe's stray service), and its fix came with the Phase 2 commits.

