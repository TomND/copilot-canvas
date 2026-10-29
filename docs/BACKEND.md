# Backend API and the shared canvas document

Everything a canvas front end needs from the machine the agents run on goes
through one interface, the backend (PLAN.md §3 "L3 Backend/Core", §6, §7). The
canvas document itself belongs to the service: it is the single writer, and
every client applies the patches it broadcasts (Phase 1, §11). Front ends
(`canvas-core` and the TUI or GUI on top of it) never touch the file system,
agent CLIs, Task Scheduler or VS Code storage; `canvas-core` no longer depends
on `canvas-agents`.

```
 front end (TUI / GUI)                      service process
┌────────────────────────────┐            ┌─────────────────────────────────────┐
│ canvas-core App            │  DocOpen   │ canvas-agents service               │
│  doc (working copy)        │──DocEdit──▶│  Docs: canvas files (Store lock),   │
│  Replica (synced, shadow)  │◀─DocPatch──│   edits in arrival order, saves     │
│  prefs file (own view)     │◀─Doc/Saved─│                                     │
│                            │            │  RPC workers ─┐                     │
│  backend: Arc<dyn Backend> │──Rpc──────▶│               ├─ backend::handle    │
│   LocalBackend (pipe) ─────┼─in-process─┼───────────────┘   (same code)       │
│   RemoteBackend (--remote) │◀─RpcReply──│                                     │
└────────────────────────────┘            └─────────────────────────────────────┘
```

## Crates

| Where | What |
|---|---|
| `canvas_model::rpc` | `Request`, `Response`, `trait Backend` (`fn call(&self, Request) -> Result<Response>`), typed helpers on `dyn Backend` (`backend.list_dir(…)`, …), `BackendInfo`, `PathInfo`, `PassiveFiles`, `Offline` (a backend that is not there) |
| `canvas_model::sync` | `Op`, `apply`, `diff`, `Replica`, `DocReply`, `Opened`, `ClientPrefs`, `doc_key`, `stable_id` |
| `canvas_model::{agent, config, automation, vscode, migrate, git}` | data types the front ends use (moved out of `canvas-agents`, which re-exports them) |
| `canvas_agents::backend` | `handle(&Ctx, Request) -> Response` (the one implementation) and `LocalBackend` (in-process) |
| `canvas_agents::service` | `Docs` (open documents), the RPC worker pool (8 threads), `DocOpen` / `DocEdit` / `Rpc` handling |
| `canvas_core::remote` | `RemoteBackend`: sends `ClientMsg::Rpc`, waits for the `ServiceMsg::RpcReply` with the same id |
| `canvas_core::prefs` | the client's own view of a canvas |

The binary wires the backend: `copilot-canvas` passes a `LocalBackend` to the
TUI; with `--remote` the TUI uses a `RemoteBackend` attached to its connection.

## Backend requests

Requests travel as JSON (`ClientMsg::Rpc { id, json }` → `ServiceMsg::RpcReply
{ id, json }`), so a request an older service does not know is answered with
`Response::Error("the service does not support this request (update it): …")`
and optional fields can be added. A request that ran and failed answers
`Response::Error(text)`; the typed helpers turn it into `Err`. `Err` from
`call` itself means the backend could not be asked (not connected, timed out,
the connection dropped while waiting).

| Request | Answer | Used by |
|---|---|---|
| `Info` | `Info(BackendInfo)`: data folder, user profile folder, Copilot history folder, `data.db` path, the machine's default agent, installed agents | App start (agent for new sessions, installed agents, suggested group folders, folder browser start) |
| `ConfigSnapshot { agent }` | `Config(Snapshot)` (problems in `errors`; plugin folders filled in as `Plugin::local_folder`) | Plugins / MCP / Skills / Agents / Settings views, MCP picker, migration |
| `ConfigSetEnabled { agent, what, name, enabled }` | `Text` (CLI output) | Space in the views |
| `ConfigRemove { agent, what, name }` | `Text` | Del in the views |
| `PluginUpdate { name }` | `Text` | plugin "Update" |
| `SkillAdd { source }` | `Text` | "Add skill…" |
| `McpAdd { line }` | `Text` | "Add MCP server…" |
| `ConfigSet { key, value }`, `ConfigUnset { key }` | `Text` | Copilot settings |
| `CliCommand { args }` | `Texts` (the `copilot` command line) | "Install plugin…" runs it in a canvas terminal (a normal host session) |
| `RestoreMouse` | `Flag` (changed) | after replacing an old local service |
| `Automations { dir, tasks }` | `Automations(AutoLoad)`: Copilot's `data.db` plus, when `tasks`, the manifest folder with Task Scheduler state; `dir` = the folder used | Automations view, boxes, refresh every 15 s |
| `AutomationsDir { dir }` | `Text` (folder in use for the canvas setting) | Automations / Settings views |
| `AutomationSetEnabled { key, enabled }` | `Done` | Space on a Copilot workflow |
| `TaskSetEnabled { name, manifest, enabled }` | `Done` (manifest first, then the task) | Space / Enable on a manifest task |
| `TaskRun { name }` | `Done` | "Run now" |
| `NewestMatches { patterns }` | `Texts` (newest file per glob that matches) | "Ask doctor" (workload logs) |
| `Tails { files: [(path, max)] }` | `Texts` | automation info page (launcher and workload log), refreshed every 2 s while shown |
| `VsCodeScan { created, since_ms }` | `VsSync { candidates, warnings }` | VS Code sync |
| `MigratePlan` | `MigratePlan(items)` | migration preview |
| `MigrateApply { items }` | `Texts` (result lines; backup under the data folder) | migration |
| `ListDir { dir }` | `Texts` (".." then folders; drives when `None`) | folder browser |
| `PathInfo { path }` | `Path(PathInfo)`: exists, folder, file, absolute form, occupied | launch (folder missing), new-session checks, worktree target, group folder, manifest folder |
| `DirsExist { paths }` | `Flags` | recent-folder validity (for front ends) |
| `ReadText { path, max_bytes }` | `Text` (at most 4 MB from the start) | a text viewer (remote mode) |
| `OpenFolder { path }` | `Done` | "Open folder" (Explorer where the backend runs) |
| `OpenTarget { target }` | `Done` | automation outputs, logs, report links |
| `GitProbe { dir }` | `Git(Option<GitInfo>)` (root, branch, local then remote branches) | new-session dialog |
| `WorktreeAdd { root, path, new_branch, base }` | `Done` | new worktree session |
| `HasHistory { agent, id }` | `Flag` | can a Copilot session be forked |
| `Fork { parent, child, name, cwd }` | `Done` | forked Copilot subsession |
| `Transcript { agent, session, parent, agent_key, cwd, limit }` | `Transcript(Transcript)`: `entries` oldest first (prompt, answer, tool call with a few words of arguments and the start of its result or exit code, note), `truncated` (only the newest part was read), `running` (an unfinished turn or open tool call), `note`. `session` is the agent's own id; `agent_key` a subagent inside its history (Copilot `agentId`, Claude Code `Agent` call id); OpenCode children are their own `session` with `parent`. Bounded: the newest 2 MiB of a JSONL history, the newest 150 OpenCode messages by index (refused without OpenCode's indexes) | the transcript inspector, every 1.5 s while the conversation runs, else every 5 s (docs/AGENTS.md) |
| `PrepareGroup { group }` | `Done` | group folder chosen, "Open AI docs / human outputs" |
| `EnsureCatalog { folder }` | `Text` (path) | "Edit events…" |
| `DoctorFolder` | `Text` (`<data>\factory`, created) | the automation doctor's group |
| `CreatePassive { files }` | `Text` (the slug used) | new passive session (rules, memory, run log, `GOALS.md`, event catalog) |
| `DocBackup { path, label, doc }` | `Text` (backup file) | before a VS Code sync, before its undo |
| `DocBackups { path }` | `Texts` (oldest first) | backups of a canvas |
| `PairCode { name }` | `Pairing(PairingCode)`: code, expiry, the remote address (listening starts if it was off) | `copilot-canvas remote pair` (the service's own pipe only) |
| `RemoteClients` | `RemoteClients(list)`: id, name, paired, last seen, revoked, open connections | `remote clients` (pipe only) |
| `RemoteRevoke { client }` | `Text` (what was revoked; its connections are closed) | `remote revoke` (pipe only) |
| `SetClipboardImage { png }` (base64 in JSON, at most 20 MB) | `Done` (the image is on the service machine's clipboard as `CF_DIB` + `PNG`) | remote image paste (Ctrl+V / Alt+V), see REMOTE.md |
| `Upload { name, id, offset, data, last }` | `Done`, the last piece `Text` (the file's path) | files dropped on a terminal in remote mode: base64 pieces of at most 4 MiB, files of at most 50 MB, written under `<home>/uploads/<date>/` with a free name |

The three remote-access requests are answered by the service itself (they
need its pairing state) and only for clients on its local pipe; TCP clients
and the in-process backend get an error.

Slow requests (CLI runs, scans, worktrees) are made on background threads and
their results come back as `AppEvent`s, as before. Quick ones (a folder
listing, a path check) are made where the answer is needed; locally they cost
what the direct call cost, remotely one round trip. Remote callers wait up to
`Request::timeout()`: 30 s, 2 min for scans and Task Scheduler, 5 min for
agent configuration, 15 min for plugin updates, migrations, worktrees and forks.

The draw path does no I/O: log tails, the automation folder, plugin folders
and the backend's paths are cached in `App` and refreshed in the background.

The service answers requests on 8 worker threads, so a slow request never
blocks the connection's reader (input and other messages keep flowing).

## The shared canvas document

### Opening

`ClientMsg::DocOpen { path, demo, cwd }` opens a canvas on the service's
machine: `path`, else `<home>\canvas.json` (`<home>\demo-canvas.json` with
`--demo`). The first client loads it under the file lock (`Store`); a missing
file starts from the demo layout (`demo`), the prototype's `layout.json`, or an
empty canvas, and is written at once. Later clients join the open document.

The answer is `ServiceMsg::Doc { key, version, json }`; `json` is a `DocReply
{ path, client, doc, error }`: the document, the canvas file's path, and the
receiving connection's id (`client`), which marks its own patches. `key` is
`<file stem>-<FNV-1a of the lower-case path>` (stable and file-name safe).
The TUI opens the document right after connecting, before it shows anything;
`--canvas FILE` and `--demo` are passed through.

A document nobody has open is saved and closed (its file lock released).

### Editing

A client changes its working copy and marks it dirty; within 100 ms
`Replica::take_edit` turns the difference to what it last sent into one edit:

| Op | Meaning on the service |
|---|---|
| `PutNode(node)` / `PutGroup(group)` | insert (appended), or replace the whole object |
| `NodeFields(node, fields)` / `GroupFields(group, fields)` | set those fields of the object; dropped when it no longer exists |
| `DelNode(id)` / `DelGroup(id)` | delete |
| `SetField(name, json)` | set a shared top-level field: `recent_folders`, `defaults`, `vscode`, `idle_stop_minutes`, `idle_stop_v2`, `mcp`, `automations_dir`, `doctor_created`, `default_agent` |

`ClientMsg::DocEdit { key, base, ops_json }` carries the list (`base`: the
version the client had). The service applies the edits of all clients in
arrival order, validates the result (an edit that breaks the document, such as
a duplicate id, is refused: the sender gets `Error` and the whole document
again), bumps the version and sends `ServiceMsg::DocPatch { key, version,
origin, ops_json }` to every client that has the document open, including the
sender, in version order (sent under the document lock).

So the last edit wins per field: one window moving a box while another renames
it keeps both; two windows renaming it keep the later name; a delete wins over
concurrent edits of the deleted box.

### Applying patches

The client's `Replica` keeps the service's document (`synced`, at `version`),
that document plus its own edits in flight (`shadow`), and which fields of
which objects are in flight. For each patch:

- its own (`origin` = its connection id): applied to `synced`; the oldest edit
  in flight is confirmed;
- another window's: applied to `synced`, to `shadow` except fields of its own
  edits in flight, and to the working copy except fields it changed itself
  that the service has not applied yet (in flight or not sent). Those arrive
  at the service later, so they win there too: every client ends up with the
  service's document.
- When nothing is in flight, boxes and groups are put in the service's order.
- A missing version (a patch was lost) makes the client ask for the whole
  document again (`DocOpen` on the same connection).

When another window overwrites a field this client changed in the last 30 s
(a box's name, position, …; not review times), the client shows "“name” was
changed in another window." The same goes for user-facing settings
(defaults, VS Code settings, MCP presets, idle stop, automations folder,
default agent).

Boxes created at the same time by several windows for the same thing
(automation boxes, the Automations group) get ids derived from what they
show (`sync::stable_id`), so they end up as one box.

### Reconnecting

After a reconnect (or a service restart) the client opens the document again.
`Replica::resync` takes the service's document and puts the client's own
changes that are not known to have arrived on top; they go out with the next
edit. While offline nothing is sent; local changes wait.

### Saving

The service writes a document 300 ms after its first unsaved change
(atomically: a temporary file renamed over the canvas, retried while
antivirus holds it), at once for a new document and for one nobody has open
any more, and when the service exits. `ServiceMsg::DocSaved { key, version,
error }` tells the document's clients when saving starts or stops failing;
the TUI shows it in the status bar and when it quits.

Quitting a client sends its last edits and waits (up to 3 s) until the
service confirmed them.

Backups (`backups\<canvas>-<ms>-<label>.json` next to the canvas, newest 30
kept) are written through `DocBackup`. Undoing the last VS Code sync restores
the client's snapshot as an ordinary edit.

The file format is unchanged: canvases from earlier versions load as they
are, and `view`, `ui`, `tiles` and `selected` stay in the file (from before)
but are no longer written by clients.

## Per-client preferences

The view (position, zoom), panels (sidebar width and visibility, terminal
share, tiled mode, collapsed groups), the recent tile order and the selected
box belong to each client. They live in the client's own data folder:
`<home>\prefs\<instance>-<document key>.json`, written 400 ms after a change
and on quit. The first time a client opens a document it starts from the
document's own (legacy) fields. Patches never carry them, so two windows on
one canvas each keep their own view.

## Protocol

All new messages are protocol 8 and appended at the end of the enums:

| Client → service | Index |
|---|---|
| `DocOpen { path, demo, cwd }` | 21 |
| `DocEdit { key, base, ops_json }` | 22 |
| `Rpc { id, json }` | 23 |
| `Pair { code_proof, name, nonce }` (the first pairing scheme; refused now) | 24 |
| `PairStart { name, nonce }` (REMOTE.md, Pairing) | 25 |
| `PairProof { proof }` (REMOTE.md, Pairing) | 26 |

| Service → client | Index |
|---|---|
| `Doc { key, version, json }` | 16 |
| `DocPatch { key, version, origin, ops_json }` | 17 |
| `DocSaved { key, version, error }` | 18 |
| `RpcReply { id, json }` | 19 |
| `Paired { client_id, nonce, secret }` (REMOTE.md, Pairing) | 20 |
| `PairChallenge { nonce, proofs }` (REMOTE.md, Pairing) | 24 |
| `Refusal { proof }` (REMOTE.md, handshake) | 25 |
| `Welcome { build }` (REMOTE.md, handshake) | 26 |

`ServiceMsg::shaped` never sends them to protocol-7 clients. The connection's
reader thread routes `RpcReply` to the waiting caller by id; a dropped
connection fails the calls waiting on it. An answer too large for one frame
(32 MiB) becomes an error answer; the connection stays up. `Tails` reads at
most 8 files and 4 MiB in all.

## Tests

- `canvas_model::sync`: concurrent edits (field-level last-writer-wins, delete
  vs. edit in both orders, bookkeeping never reverting a rename), three windows
  with interleaved and delayed delivery, 200 random interleavings with lost
  edits and resyncs: every window converges on the service's document,
  including order.
- `canvas_core::app`: edits leave as `DocEdit`, other windows' patches apply
  with notices, a missing patch resyncs, local changes survive the resync,
  save errors are shown. `canvas_core::remote`: answers reach their callers,
  dropped connections fail pending calls.
- `canvas_agents::service::docs`: open/join, edit order, refused edits, save,
  close and lock release, broken files.
- `tests/backend_contract.rs` starts a real service (`COPILOT_CANVAS_LISTEN=
  127.0.0.1:0`, temporary home) and runs one contract suite against the
  in-process backend and against the service over loopback TCP (folder
  browsing, paths, text and tails, Git probe and worktree in a temporary
  repository, group folders, catalog and passive files, automations from the
  sample `data.db` and toggling, fork of a synthetic Copilot history and its
  transcript, backups,
  errors); both must give the same answers. It also checks that an unknown
  request leaves the connection usable, that calls fail once the service is
  gone, and that two clients editing one canvas through the service converge
  (and that the service saves it and refuses a breaking edit).
  `config_snapshot_is_the_same_locally_and_remotely` needs an installed agent
  (`cargo test --test backend_contract -- --ignored`).
- `two_clients_probe` (manual: `cargo test --release two_clients_probe --
  --ignored`): two TUI clients in hidden pseudoconsoles, one over `--remote
  loopback`, one over the pipe; a rename in either shows in the other, panning
  one leaves the other's view alone.
- `selftest`, `selftest --remote loopback`, `selftest --remote faults` and
  `selftest agent` run unchanged.

## Limitations

- `OpenFolder` / `OpenTarget` open Explorer where the backend runs; front ends
  only use them locally. With a service on another machine, links, folders,
  automation files and passive-session files open on the home PC as described
  in REMOTE.md ("Local vs remote behaviours"): URLs in the home browser, the
  Cloud PC's files in VS Code through a tunnel or in the canvas's viewer.
- Quick requests made on the UI thread cost one round trip in remote mode
  (folder browsing, launching, the new-session checks).
- A paired remote client is trusted like the local user: its requests can
  read text files, write backups and run the agents' CLIs on the service's
  machine (it can already run anything in a terminal).
- An old service (protocol 8 from before this change) cannot open canvases:
  the client says so and asks to restart it.
