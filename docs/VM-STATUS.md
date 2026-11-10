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

### F: foundations
- [ ] VM-F1 satellite protocol (`canvas_common::sat`), `CAP_SATELLITE`, `ClientMsg::Sat` / `ServiceMsg::Sat` at 29, shaping
- [ ] VM-F2 `Credential::Satellite`, `Accepted::satellite`, refused on the pipe
- [ ] VM-F3 model types (`Placement`, `VmPlacement`, `canvas_model::vm`, `Snapshot::vm`, `AgentPolicy::start_on_vm`, launch carrier and `vm:` agents, `Vm*` requests)
- [ ] VM-F4 `settings.json` `vm.pack`, `ATELIER_VM_PACK`
- [ ] VM-F5 `canvas_agents::vm` skeleton
- [ ] Exit: clippy (macOS), `cargo xwin clippy`, the portable line, the touched tests; CI green; merged into main

### M: thin lane
- [ ] VM-M1 pack loader, `atelier vm pack check`, example pack (`docs/examples/vm-pack/`)
- [ ] VM-M2 hooks runner and `atelier vm-hook-standin`
- [ ] VM-M3 placement in launches (fail closed)
- [ ] VM-M4 `atelier vm attach`
- [ ] VM-M5 `atelier satellite --lite`, guest outbox
- [ ] VM-M6 hub poller, the `CliOp` executor (first verbs)
- [ ] VM-M7 status from mirrors (liveness overlay)
- [ ] VM-M8 minimal UI (*Run on*, ☁ marker, *Start on VM…*), only with a pack
- [ ] VM-M9 docs
- [ ] Exit: `selftest vm --lane hooks` on Windows, macOS, Linux; no-pack snapshots and self-test unchanged; merged into main

### 0: spike kits
- [ ] `atelier vm spike s1…s5, s7` with local analogs in CI; S6 text; `docs/VM-OWNER-CHECKLIST` part below

### 1: manager
- [ ] VM-11 provider trait · VM-12 `vm::aca` (HTTP stand-in) · VM-13 `vm::local` · VM-14 stores · VM-15 manager (images, pool, budgets) · VM-16 launcher · VM-17 claim · VM-18 `VmView`, `Vm*` requests, `atelier vm …` · VM-19 `has_work`, doctor
- [ ] Exit: as in the plan §3 Phase 1

### 2: terminals
- [ ] VM-21 remote PTY · VM-22 VT repaint · VM-23 satellite · VM-24 hub link · VM-25 launch, shutdown, re-adoption · VM-26 `ExecTarget` · VM-27 `SessionStore` · VM-28 guest subsessions · VM-29 gap and reconnect rules · VM-2A docs
- [ ] Exit: `selftest vm` (replay, gap, freeze, restart) on three OSes

### 3: integration
- [ ] VM-31 `Io` seam · VM-32 CLI satellite mode · VM-33 op relay · VM-34 file sync · VM-35 profile and path map · VM-36 egress and secrets · VM-37 MCP gateway · VM-38 guest MCP config · VM-39 docs and skills
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
- [ ] egress isolation: a guest process that ignores proxy settings is still refused
- [ ] `atelier selftest vm --real` once per agent (Phase 5)

## Log

- 2026-11-09: worktree and branch made from `origin/main` 3f6551e; the plan copied into
  docs/VM-SESSIONS-PLAN.md with the approval; this file.
