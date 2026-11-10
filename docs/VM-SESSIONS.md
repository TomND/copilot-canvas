# VM sessions: running agent sessions on microVMs (design)

Status: design, 2026-11, approved for implementation 2026-11-09 (owner: Thomas), every phase (M, 0–5), with
the Azure provider built against its documented REST API and tested in CI through the `local` provider; checks
that need real sandboxes are the owner's (`selftest vm --real`, spikes S1–S7). Targets `main` @ `fac3c4d` or later.

**VM sessions are optional; local stays the default** (owner, 2026-11-09). Without a VM pack nothing changes
and no VM UI shows (no *Run on* field, no ☁ panel; `atelier vm pack check` only says that no pack is
configured). With a pack, *Run on* defaults to *This machine* and `Placement` defaults to `Local` everywhere (the
box, the canvas's and a group's launch defaults, a board's policy): a VM is chosen only explicitly, per session,
as a launch default, or by a board's *Start on VM*. Old documents and canvases behave exactly as before.

The implementation plan is [VM-SESSIONS-PLAN.md](VM-SESSIONS-PLAN.md) (approved with its ten decisions); progress,
the owner's checklist and where the implementation departs from the plan are in [VM-STATUS.md](VM-STATUS.md).

This document is generic. It names no organization, subscription, repository, host or credential. Everything
specific to one user or company lives in a **VM pack**: a folder of configuration outside this repository (§5),
which the canvas reads at runtime. The repository ships the schema, examples using `example.com`/`contoso`
placeholders, and tests. It ships no defaults that point at real infrastructure.

## 1. Goal

From the canvas, start a session or a todo **on a microVM** as easily as on the local machine. The box behaves
like any other session:
- live terminal, status, subagents and transcript;
- events, messages, PR coordination, todos, notes and reports;
- the skills, instructions, MCP servers and group context the pack allows.

Each session gets an isolated Linux VM with its own clone and branch. Code is edited and pushed there, and CI is
expected to build and test it (a code-only lane). The machine running the canvas service (the **hub**) spends
little CPU per VM session.

Non-goals (first version):
- Windows guests, or building in the guest by default.
- Running the canvas UI in the guest.
- Providers other than Azure Container Apps Sandboxes (the provider is a trait, §4.4).

## 2. Platform facts (Azure Container Apps Sandboxes, public preview; from its public docs and a test group)

- Linux microVMs in a regional *sandbox group*. Sizes go up to 4 cores, 8 GB and 80 GiB. Boot takes a few
  seconds; resume from a memory suspend takes under a second. Snapshots and disk images (from OCI images or by
  committing a running sandbox) are supported.
- **A committed image keeps its builder's disk size as its minimum disk.** Build images on the smallest disk the
  smallest sandbox size allows.
- The data plane is REST at `https://management.<region>.azuredevcompute.io/...` (api-version
  `2026-02-01-preview`), with Entra tokens for audience `https://management.azuredevcompute.io`. The `aca` CLI
  costs about 4 s per call (token acquisition), so the canvas calls REST directly with a cached token.
- An **egress proxy** with default Allow or Deny, host/path/method rules, and `Transform` rules that set headers
  from group **secrets** or managed-identity tokens. The value never enters the guest. Secrets are not
  environment variables.
- `aca sandbox shell -c CMD` gives an interactive PTY. Ports can be published anonymously or restricted to one
  signed-in email. Labels support selection. The default quota is 300 concurrent cores per region per
  subscription.

## 3. Architecture

```
 HUB (the canvas service's machine: unchanged owner of state)        GUEST (one microVM per branch)
 ┌──────────────────────────────────────────────────┐               ┌──────────────────────────────────────────────┐
 │ atelier service                            │               │ atelier satellite (same binary, Linux) │
 │  L1: Terminal { pty: Local | Remote }   ◀─────────┼── satellite ──┼─▶ PTY owner (agent CLI), generations, ring    │
 │  L3: doc, bus, todos, PRs, notebook, reports      │   link (WSS,  │   headless emulator (gap snapshots)           │
 │  SessionStore: local roots + guest mirrors  ◀─────┼── outbound ───┼─ history shipper                              │
 │  vm::Manager (provider trait), Launcher,          │   from guest) │  local endpoint for the canvas CLI (relay)    │
 │    pool, images, secrets, budgets                 │               │  file sync (down: bundles; up: notes, outputs)│
 │  vm::McpGateway (policy-checked)            ◀─────┼───────────────┼─ MCP proxy http://127.0.0.1:<port>/mcp/<name> │
 │  remote listener (WebSocket) behind Dev Tunnel    │               │ egress proxy injects host credentials (pack)  │
 └──────────────────────────────────────────────────┘               └──────────────────────────────────────────────┘
```

**A1. The hub stays the single owner.** The document, bus, todo book, PR monitor, notebook and reports stay
where they are. The guest becomes a source of truth for nothing.

**A2. The terminal is drawn on the hub; the PTY is in the guest.** `Terminal` gets a `Remote` PTY kind fed by
the satellite. The emulator, frames, OSC 9;4 status, scrollback, paste and message typing stay unchanged.

**A2b. Compute continues; integrations wait.** When the hub is offline, the guest's agent keeps working, but
hub-owned operations (MCP, todo writes, PR coordination, credential refresh) do not happen. The satellite keeps
durable, acknowledged outbound queues. Guest commands answer *queued (hub offline)*, never a false success, and
MCP calls fail fast. The box shows *integration offline since …*.

**A2c. Terminal generations.** Each spawn gets a hub-assigned generation. Input is acknowledged only once it is
written to the guest PTY, and output carries `(generation, seq)`.
- A reconnect within the ring (default 4 MB) replays the missing output.
- Otherwise the hub resets the emulator from the satellite's screen snapshot and marks a gap.
- Queued messages are typed in only after status is reconciled.
- History files are identified by `(session, file, inode, generation)`.

**A3. The link is outbound from the guest**, to the hub's existing WebSocket listener through its Dev Tunnel web
forwarding. The tunnel access token is injected by egress from a pack-configured secret.
- **Satellite credential.** The satellite authenticates with a per-sandbox credential (pairing-style id + secret)
  minted at claim and revoked on delete.
- **The `Satellite` role is decided from the credential during the handshake, before any terminal list,
  snapshot or document is sent.** Satellites are excluded from broadcasts.
- **What L1 and L3 accept.** L1 refuses `Launch`/`Input`/`Subscribe` from a satellite. L3 accepts only `Sat*`
  messages for that sandbox's assigned terminals.
- **Capabilities.** Client capabilities never grant anything.

**A4. The control plane uses provider REST with a cached token.** No CLI subprocess is on the launch path.

**A5. Data is data.** Relayed events, comments and messages keep the canvas's existing untrusted-data rules.

## 4. Canvas changes

### 4.1 `canvas-model`
- `Placement { Local | Vm { repo, branch, size } }` on `Node`, in `LaunchDefaults` (canvas and group), and in a
  board policy (*Start on VM*). `repo` is a key in the pack's repo catalog, never a URL.
- `VmInfo` in the bus snapshot: state, size, image age, uptime, estimated cost, git ahead/behind/dirty, and
  integration status.
- RPCs: `VmList`, `VmPool`, `VmImages`, `VmAction { box, op }`, `VmImageBuild { repo }`, `VmBudget`,
  `VmPackStatus`.

### 4.2 `canvas-common`
- Appended `ClientMsg`/`ServiceMsg` `Sat*` variants (§6), `CAP_SATELLITE`, and a satellite role on stored
  credentials.
- A launch carrier `ATELIER_PLACEMENT` (an environment JSON the service strips, as with
  the skill folders' carrier `canvas_model::launch::GROUP_SKILLS_ENV`, which keeps its wire name
  from before the rename; a new carrier takes the `ATELIER_` name, and an older service passes
  it on to the agent, where it does nothing).

### 4.3 `canvas-host`
- A `Remote` PTY kind next to the local `portable-pty` one.
- The priority manager and process-tree logic skip remote terminals, and `stop()` maps to `SatStop`.

### 4.4 `canvas-agents`
- **`vm::Provider` trait** (create from an image, egress apply, exec, suspend, resume, delete, list by label,
  image build/commit, secret set). The first implementation is `vm::aca`; a `vm::local` implementation runs the
  satellite as a local process in a temp home, for tests and CI.
- **`vm::Manager`**: images (nightly rebuild, keep N), warm pool with leases (credentials injected only after a
  claim; members recycled after a max age, never returned), budgets and quota.
- **`vm::Launcher`**: durable launch intents (`<home>/vm/launches.json`) and an idempotent state machine:

  claim → credentials → bundles → spawn → attached.

  Fencing generations, cancel/retry, reconciliation at start (adopt or clean labelled orphans), and rollback.
  `resolve_agent` records the intent and returns at once; it never provisions.
- **`ExecTarget { Local | Vm { sandbox, roots, path_style } }`**, chosen before the command is built. Providers
  build the command (executable, cwd, `--plugin-dir`, `--add-dir`, instruction dirs) for the target, and guest
  PIDs are kept apart from host PIDs.
- **`SessionStore::resolve(box)`** is used by every history consumer: index polling per root, `Transcript`,
  `HasHistory`, `Fork`, `is_live`, subsessions. Guest liveness comes from satellite process reports, never from
  mirrored lock files.
- **`vm::McpGateway`** (§7).
- **`atelier satellite`** (Linux): link, PTY host, generations, ring, headless emulator, history shipper,
  CLI endpoint, file sync, MCP proxy, stats.
- **CLI**:
  - `atelier vm list|pool|images|image build REPO|shell BOX|suspend|resume|delete|budget|pack check`;
  - in the guest, the existing commands run in `ATELIER_REMOTE=satellite` mode. `note`, `report`, `pr`
    and `todo` call the satellite endpoint, which forwards acknowledged hub operations. Each command's file and
    liveness contract is inventoried first.

### 4.5 `canvas-core`, TUI and GUI
- New-session **Run on** (this machine / VM): repo (from the pack's folder → repo map), branch, size, MCP preset.
- Box ☁ states: starting, suspended, integration offline, lost. A VM section in the details panel with Suspend,
  Resume, Open shell, *Update from main* (a message to the agent) and Delete.
- Tasks: **Start on VM** (branch from the todo id, claim, link, first prompt that runs `todo prime` / `show` /
  `submit`).
- A ☁ VMs view: pool, images, quota, cost and pack status. Parity rows for each.

## 5. The VM pack (configuration outside the repository)

Settings: `vm.pack` (a folder path) or `ATELIER_VM_PACK`. Without a pack, VM features are hidden.
`atelier vm pack check` validates it. The schema is versioned (`pack_version: 1`).

| File | Holds |
|---|---|
| `vm.yaml` | provider (`aca`), subscription, resource group, sandbox group, region; sizes; pool minimums and max age; image schedule and keep-count; budgets (max running, max cores, daily soft budget); suspend-after; tunnel secret name; git identity |
| `repos.yaml` | catalog: key → clone URL, default branch, clone mode (`blobless`, `single_branch`, `sparse` patterns), image base, image disk, egress profile, folder globs that map local folders to this key |
| `egress/*.yaml` | egress policy templates per profile, with placeholders (`{{box.branch}}`, `{{box.repo}}`, `{{box.id}}`, `{{sandbox}}`, `{{hub.host}}` in hosts and paths; `{{secret:NAME}}` only in header values) filled at claim: deny by default, host/path/method rules, transforms that inject secrets |
| `secrets.yaml` | group secrets and how the hub refreshes them: a command whose stdout becomes the value (never logged or written to disk), a key name, an interval. The canvas pipes stdout to the provider's secret API |
| `mcp-policy.yaml` | the MCP servers guests may use (by the hub's configured server name), process mode (`per_session` default, `shared` opt-in), and default-deny rules over tool + action + argument constraints (with placeholders such as `{{box.branch}}`, `{{box.repo}}`) |
| `profile.yaml` | allowlist of personal agent-profile paths to bundle (skills, instructions, agents), keys allowed from agent config, and paths always excluded; no credentials ever |
| `paths.yaml` | path map: host prefixes → guest paths, with sync mode (`ro-snapshot`, `rw-notes`, `none`) and an explicit allowlist; generates the guest's path-map instruction |
| `hooks/` (optional) | external commands for the Phase M lane (§10): `claim`, `shell-command`, `suspend`, `resume`, `delete`, `image` |

Rules:
- Pack values are data. Commands in `secrets.yaml` and `hooks/` run as the user and are shown in
  `pack check`.
- The canvas never copies a pack into the repository, the canvas document, logs or reports.

## 6. Satellite link messages

| Direction | Message | Purpose |
|---|---|---|
| sat → hub | `SatHello { version, caps }` (role from the credential) | session start |
| hub → sat | `SatSpawn { term, generation, argv, env, cwd, cols, rows }` | start an agent under a PTY |
| hub → sat | `SatInput { term, generation, seq, bytes }`, `SatResize`, `SatSignal`, `SatStop` | control |
| sat → hub | `SatInputAck`, `SatOutput { term, generation, seq, bytes }`, `SatSnapshot`, `SatExit`, `SatProc` | output, replay, gap reset, liveness, nested sessions |
| sat → hub | `SatHistory { root_id, file_id, offset, bytes }` | history appends into the hub mirror |
| sat ↔ hub | `SatOp { id, op }` / `SatOpResult { id, result }` | acknowledged CLI operations (note, report, pr, todo, emit, send, status, effect) |
| hub → sat | `SatFile`, `SatDelete`, `SatManifest` | down-sync of bundles, guest routing snapshot, todo book, MCP config |
| sat → hub | `SatNote { root_id, name, base_rev, bytes }` | up-sync of notes and outputs |
| sat ↔ hub | `SatMcp { stream, bytes }` | MCP gateway streams |
| sat → hub | `SatStats` | resources, git state |

Rules:
- **Priority queues.** Input and output chunks (≤ 16 KB) go ahead of everything else.
- **Limits.** Per-sandbox in-flight and memory limits; rate limits on history and files.
- **No guest-supplied paths, roots or identities.** The hub assigns root ids and keeps the terminal ↔ session ↔
  sandbox mapping.
- **Names.** Names are single components checked by the notebook's containment rules. Links, special files,
  `..`, absolute paths and Windows aliases are refused.
- **Negative tests** cover traversal and cross-sandbox writes.

## 7. MCP gateway

- The hub runs MCP servers for guests **lazily, one process per guest session and server** by default, with the
  hub's own configuration and credentials.
- Sharing across sessions is opt-in per server, only after tests of initialize, notifications, cancellation,
  auth context and server-initiated requests. Protocol versions are pinned.
- **Default deny over server + tool + action + arguments.** `tools/list` is filtered, every `tools/call` is
  validated, unknown operations and parameters are refused, and refusals are shown in the VM details.

## 8. Lifecycle, cost, safety

- **Suspend, don't stop.** Idle and unseen for `suspend_after`, a session is suspended with its memory. Start,
  typing, a queued message or a follow-up resumes it.
- **Deletion behind a quiescence barrier:**
  1. stop new work;
  2. check every branch against its upstream, plus stashes, untracked files and uncommitted changes;
  3. drain the queues with acknowledgements;
  4. delete.

  Anything ambiguous asks the user. v1 asks for every automatic deletion. Merged or closed state is re-checked
  with the forge's CLI, not taken from an event.
- **Budgets** queue starts and say why; they never kill running work.
- **Egress** is deny-by-default at creation, with token-carrying rules scoped by host, path and method.
  *Egress tests* try forbidden calls from a guest.
- **Secrets.** Forge and tunnel credentials stay in egress injection. The only secrets inside a guest are the
  satellite credential and, if the agent CLI needs it, its own token, both injected after claim. Images and pool
  members never hold credentials.
- **Preview platform.** A guest can be lost. Work lives in pushed branches and history in the hub mirror;
  *resume on a new VM* uploads the session history.

## 9. Pool, images and `main`

- **Images.** Rebuilt on a schedule (default nightly), with the clone at the default branch; keep the last N.
- **Pool members.** Booted from the newest image, fetched, satellite started, suspended. Recycled after a max age
  (default 12 h) or when a newer image exists.
- **A claim** resumes a member, fetches the default branch, then creates the branch from it (or tracks an
  existing remote branch). A running branch is never rebased by the canvas.
- **Target:** first prompt p50 ≤ 5 s, p95 ≤ 10 s, measured end to end.

## 10. Phases

| Phase | Content | Estimate (agent-days) |
|---|---|---|
| **M** thin lane | VM boxes as command terminals, using the pack's `hooks/` (claim → `shell-command`, typically `aca sandbox shell -c "tmux new -A -s BOX '<agent cmd>'"`) and `LaunchKind::Command`; `satellite --lite` polling history and CLI ops over exec; Start on VM; egress hardening; agent CLI auth | 4–6 |
| 0 spikes | S1 link over the tunnel with the role decided before snapshots · S2 agent CLI auth in a guest · S3 Linux build in a stock image · S4 end-to-end launch p50/p95 · S5 MCP policy and per-session processes · S6 the user's data-handling policy for the pack (documented as a pack prerequisite, not decided here) · S7 failure injection | 3–4 |
| 1 manager | provider trait + `aca` + `local`, pack loader and `pack check`, images, pool with leases, secrets refresher, Launcher, budgets, `VmInfo` | 5–6 |
| 2 terminals | `ExecTarget`, satellite, role in the handshake, `Remote` PTY, generations, `SessionStore` | 7–9 |
| 3 integration | CLI contract inventory + satellite mode, file sync with root ids, profile bundle + path map, MCP gateway | 7–9 |
| 4 UX | Run on, box states, details, Start on VM, ☁ VMs view, lifecycle | 4–5 |
| 5 hardening | `selftest vm` (local provider in CI; `--real` owner-run), soak, resume on a new VM, OpenCode and Claude Code, docs and parity | 5–7 |

Phase M is useful on its own; its measurements re-estimate the rest (about 35–45 days after M).

## 11. Testing

- `vm::local` provider plus a local satellite in a temp home for CI on all platforms: launch, replay, gap
  reset, CLI operations, file-boundary negatives, MCP policy (refused vote and update, allowed draft-create for
  the box's own branch, with a stand-in MCP server), quiescence barrier.
- `selftest vm --real` needs a pack. It is run by the owner, never in CI, and never prints pack values.
- Example pack under `docs/examples/vm-pack/`, using `contoso`/`example.com` values only.

## 12. Risks

| Risk | Mitigation |
|---|---|
| The tunnel path from guests is blocked | fallback transports (port restricted to one email; streaming exec); same protocol |
| Agent CLI token must live in the guest | inject after claim; scoped token; deny-default egress |
| Hub MCP process count | per-session lazy start, small presets, opt-in sharing |
| Skills that assume the hub's OS or paths | pack path map; a `requires:` front-matter hint so such skills stay on the hub |
| Preview API changes | one provider module, version in the pack, `doctor` check |
| Pack values leaking into the repo or logs | pack outside the repo; redaction; `pack check` warns on a pack inside a git work tree that has a public remote |

## 13. As built

Phase by phase; the details and the log are in [VM-STATUS.md](VM-STATUS.md).

- **Foundations.** The satellite link's messages travel inside `ClientMsg::Sat` / `ServiceMsg::Sat` (index 29,
  `CAP_SATELLITE = 1 << 6`, their own protocol `canvas_common::sat`); a satellite's credential decides its role in
  the handshake (TCP only) and the host hands it to `HostApp::serve_satellite` before anything a client gets.
  `canvas_model::vm` holds `Placement` (absent: local), the VM view of the bus snapshot, the operations and the
  relayed CLI operations; a VM launch names its agent `vm:<kind>` and carries `ATELIER_PLACEMENT`, so a service
  without VM sessions refuses it, and the service refuses to start here a box the canvas places on a VM.
- **Phase M (the thin lane).** `atelier vm pack check` and `atelier config vm.pack`; the pack's hooks (JSON in and
  out, time limits, output never logged); `atelier vm attach` (the box's terminal: claim, then the hook's shell into
  the guest); in the guest, canvas commands are relayed through an outbox (`ATELIER_REMOTE=satellite-lite`) and
  `atelier satellite --lite` drains it with the agent's Copilot history; the service drains each running VM box
  every `poll_seconds`, mirrors the history under `<home>/vm/mirrors/<sandbox>/` (indexed like Copilot's own folder)
  and runs each operation as its box with this program. The new-session dialog's *Run on* and Tasks' *Start on VM…*
  appear only with a pack. `atelier selftest vm` runs it all on `vm::local` sandboxes answered by
  `atelier vm-hook-standin`, in CI on Windows, macOS and Linux.
- **Phase 1 (the manager).** `vm::provider` (the trait: `vm::aca`, `vm::local` and the pack's hooks implement it),
  the stores under `<home>/vm/` (`hub.json`, `satellites.json`, `sandboxes.json`, `images.json`, `budget.json`,
  `launches/`), satellites' credentials (`sat-` ids sealed for the home, minted at a claim and written to the
  guest's `~/.atelier/satellite.json`, revoked when the sandbox goes; the handshake tells them from paired
  clients), the manager's decisions (budgets queue starts with the reason and never stop running work; images on
  their schedule (UTC), the newest `keep` kept; the warm pool kept full from the newest image, its members leased
  once and recycled after `pool.max_age` or a newer image) and the launcher's steps (`queued` → `claiming` →
  `starting` → `running`, suspend, resume, delete; fenced, rolled back, three failures fail it; reconciliation of
  lost boxes, orphans and builders at start). The guest-side commands `atelier satellite --write-file`,
  `--read-file`, `--clone`, `--prepare` keep the hub's steps free of shells. `atelier vm
  list|pool|images|budget|image build|suspend|resume|delete|cancel|retry` and `doctor`'s VM lines (only with a pack).
  The providers share one contract: a guest path is relative to the guest user's home or absolute; `put_file`
  makes the folders and sets the mode (0600 for the credential, 0755 for the satellite, which image builds put at
  `/opt/atelier/bin/atelier`, first on `PATH` for `aca`'s commands); `get_file` refuses a file larger than asked
  rather than truncating it. `vm::aca` writes through a staging upload and one `install` command, and learns a
  sandbox's home once. `vm::local` keeps a sandbox's whole disk in its `home/` (the workspace `~/work/<repo>`),
  runs this build as the satellite, and runs a box's agent in the sandbox directly until the satellite link
  (Phase 2). The spikes (`vm spike s2|s3|s4`) run through whichever provider the pack names (S3 uploads the
  pack's satellite first). `atelier selftest vm` adds the pool lane on `provider: local`: the image built in a
  builder sandbox, a warm VM leased by a box and prepared on its branch, the pool filled again, the VM deleted.
- **Phase 2 (terminals; in progress).** The satellite (`atelier satellite`, started in the guest by the launcher:
  `--start` runs it in the background once; `vm::local` starts this build as a process of the sandbox) dials the
  hub (`wss://` with rustls, the platform's roots and the pack's CA; `ws://`; `tcp://`; through a `CONNECT` proxy),
  authenticates with its credential and runs each box's PTY in the guest with the alacritty engine, which answers
  the program's queries there; its raw output is kept in a 4 MiB ring with byte offsets. On the hub a VM box is a
  remote terminal (`Terminal::spawn_remote`): it shows the launch's steps as dim lines, starts the program once the
  launch runs and the satellite is linked, draws the guest's output, sends typing numbered and kept until
  acknowledged, and refuses typing while the VM is offline. A dropped link is replayed from what the hub has, or
  repainted from the guest's screen when the ring no longer reaches back; a restart of the service lets the
  terminal go and adopts it again; a suspended VM's link drops and comes back on resume. History, status and
  relayed commands use the thin lane's drain, sent over the link instead of a command per poll. `HasHistory`,
  `Transcript` find a VM session's history in its mirror; forking one is refused. The satellite reports the
  sessions an agent starts inside its terminal (they become the box's subsessions), which sessions are alive, and
  every 30 s the guest's load and each repository's git state (`vm list` shows it); the hub holds each link to
  limits (sizes, a byte rate, report rates, MCP streams). `atelier selftest vm` adds the
  pool lane on `provider: local`: the warm VM leased, its satellite linked over `ws://` on loopback, typing, status
  from the guest's history, a restart of the service, a freeze and a resume, a stop and a new launch in the same
  VM, the VM deleted.

