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
| `canvas_model::{notebook, todos}` | note provenance, front matter and the notebook requests' types; the todo book, its operations (`TodoOp`) and rules, applied alike by the service, the windows and the phone |
| `canvas_agents::notebook`, `canvas_agents::service::todos` | the notebook requests on the service's machine; the todo hub (the routed canvas's book, its single writer) |
| `canvas_agents::backend` | `handle(&Ctx, Request) -> Response` (the one implementation) and `LocalBackend` (in-process) |
| `canvas_agents::service` | `Docs` (open documents), the RPC worker pool (8 threads), `DocOpen` / `DocEdit` / `Rpc` handling |
| `canvas_core::remote` | `RemoteBackend`: sends `ClientMsg::Rpc`, waits for the `ServiceMsg::RpcReply` with the same id |
| `canvas_core::prefs` | the client's own view of a canvas |

The binary wires the backend: `atelier` passes a `LocalBackend` to the
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
| `Info` | `Info(BackendInfo)`: data folder, user profile folder, Copilot history folder, `data.db` path, the machine's default agent, installed agents | App start (agent for new sessions, installed agents, suggested group folders, folder browser start): asked at once only the first time; later starts use the copy the last run kept (`<home>/prefs/<client>-<key>.backend.json`) and ask again in the background |
| `ConfigSnapshot { agent }` | `Config(Snapshot)` (problems in `errors`; plugin folders filled in as `Plugin::local_folder`) | Plugins / MCP / Skills / Agents / Settings views, MCP picker, migration |
| `ConfigSetEnabled { agent, what, name, enabled }` | `Text` (CLI output) | Space in the views |
| `ConfigRemove { agent, what, name }` | `Text` | Del in the views |
| `PluginUpdate { name }` | `Text` | plugin "Update" |
| `SkillAdd { source }` | `Text` | "Add skill…" |
| `McpAdd { line }` | `Text` | "Add MCP server…" |
| `ConfigSet { key, value }`, `ConfigUnset { key }` | `Text` | Copilot settings |
| `CliCommand { args }` | `Texts` (the `copilot` command line) | "Install plugin…" runs it in a canvas terminal (a normal host session) |
| `RestoreMouse` | `Flag` (changed) | after replacing an old local service |
| `Automations { dir, tasks }` | `Automations(AutoLoad)`: Copilot's `data.db` plus, when `tasks`, the manifest folder with Task Scheduler state; `dir` = the folder used | Automations view, boxes, refresh every 15 s while they show, else every 2 minutes |
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
| `ListEntries { dir }` | `Texts` (".." then subfolders ending in `\`, then files; at most 5,000) | the remote folder browser (group folders, folder links); older services answer "does not support" and the browser lists `ListDir` |
| `PathInfo { path }` | `Path(PathInfo)`: exists, folder, file, absolute form, occupied | launch (folder missing), new-session checks, worktree target, group folder, manifest folder |
| `DirsExist { paths }` | `Flags` | recent-folder validity (for front ends) |
| `ReadText { path, max_bytes }` | `Text` (at most 4 MB from the start) | a text viewer (remote mode) |
| `OpenFolder { path }` | `Done` | "Open folder" (Explorer where the backend runs; local clients only: the service logs `open folder for a client: …` if a remote one ever asks) |
| `OpenTarget { target }` | `Done` | automation outputs, logs, report links |
| `GitProbe { dir }` | `Git(Option<GitInfo>)` (root, branch, local then remote branches) | new-session dialog |
| `WorktreeAdd { root, path, new_branch, base }` | `Done` | new worktree session |
| `HasHistory { agent, id }` | `Flag` | can a Copilot session be forked |
| `Fork { parent, child, name, cwd }` | `Done` | forked Copilot subsession |
| `Transcript { agent, session, parent, agent_key, cwd, limit }` | `Transcript(Transcript)`: `entries` oldest first (prompt, answer, tool call with a few words of arguments and the start of its result or exit code, note), `truncated` (only the newest part was read), `running` (an unfinished turn or open tool call), `note`. `session` is the agent's own id; `agent_key` a subagent inside its history (Copilot `agentId`, Claude Code `Agent` call id); OpenCode children are their own `session` with `parent`. Bounded: the newest 2 MiB of a JSONL history, the newest 150 OpenCode messages by index (refused without OpenCode's indexes) | the transcript inspector, every 1.5 s while the conversation runs, else every 5 s (docs/AGENT-PROVIDERS.md) |
| `PrepareGroup { group }` | `Done` | group folder chosen, "Open AI docs / human outputs" |
| `EnsureCatalog { folder }` | `Text` (path) | "Edit events…" |
| `DoctorFolder` | `Text` (`<data>\factory`, created) | the automation doctor's group |
| `CreatePassive { files }` | `Text` (the slug used) | new passive session (rules, memory, run log, `GOALS.md`, event catalog) |
| `DocBackup { path, label, doc }` | `Text` (backup file) | before a VS Code sync, before its undo |
| `DocBackups { path }` | `Texts` (oldest first) | backups of a canvas |
| `PairCode { name }` | `Pairing(PairingCode)`: code, expiry, the remote address (listening starts if it was off) | `atelier remote pair` (the service's own pipe only) |
| `RemoteClients` | `RemoteClients(list)`: id, name, paired, last seen, revoked, open connections (`uploads`: how many of them are requests-only upload connections; new field, absent from older services) | `remote clients` (pipe only) |
| `RemoteRevoke { client }` | `Text` (what was revoked; every connection of it is closed, listed or not: its upload connections too) | `remote revoke` (pipe only) |
| `SetClipboardImage { png }` (base64 in JSON, at most 20 MB) | `Done` (the image is on the service machine's clipboard as `CF_DIB` + `PNG`) | remote image paste (Ctrl+V / Alt+V), see REMOTE.md |
| `Upload { name, id, offset, data, last }` | `Done`, the last piece `Text` (the file's path) | files dropped on a terminal in remote mode: base64 pieces (any size; clients send 256 KiB since round 1, 4 MiB before), files of at most 50 MB, written under `<home>/uploads/<date>/` with a free name. Clients send uploads, and clipboard images over 256 KiB, on a short-lived connection of their own (`conn::Bulk`: another client with its own session that shows nothing), so the window's typing and heartbeats never wait behind them |
| `CreateCanvasAgent { files }` | `Text` (the slug used) | a new canvas-wide agent (the *New canvas agent* dialog; `rules` = its Instructions): `rules.md`, `state.md`, `log.md` in `<home>/canvas-agents/<canvas>/agents/<slug>` (the first free slug), the canvas's `GOALS.md` and `skills/` when missing. `files` names the canvas by its id (letters, digits, `-`), never a folder |
| `CanvasSkills { canvas }` | `Skills(list)`: name, description, SKILL.md path and any problem per skill | the canvas-agents panel, an agent's *Skills…*, the skill library |
| `CreateCanvasSkill { canvas, name, description }` | `Text` (its SKILL.md) | *New skill…* (never over an existing skill; the name is checked) |
| `Reports` | `Reports(Snapshot)`: the inbox (newest first, at most 600 rows, with read state and the refused submissions) | the Reports view before the service's first push (normally `ServiceMsg::Reports` brings it) |
| `ReportContent { id }` | `Report(Content)`: the report and its body (Markdown, HTML or text; at most 2 MiB) | the report viewer, on open; also over `--remote`, so HTML is shown by the home GUI |
| `NotebookInfo { root, canvas }` | `NotebookInfo(Info)`: `root` (the canvas setting, else `<home>/notebook`; created with `Groups/`, `Inbox/`, `Todos/`), `canvas_dir` (`<home>/canvas-agents/<canvas>` for a valid canvas id, else none), `home` | the Notebook (its roots), quick capture, spec notes, exports |
| `NotebookList { dir, root }` | `NoteEntries(Vec<Entry>)`: name, absolute path, `rel` (to `root`, `/` separators), kind, size, modification time, `Provenance` and title (metadata from the first 8 KiB); folders first, then by name; at most 5,000 (of the first 50,000 names read); names starting with `$` and `System Volume Information` left out. `root` (optional; `None`: `dir` itself) is the explorer root the entries are classified under; a root inside `<home>/canvas-agents` is the canvas-wide folder | the explorer (lazily, as folders open, and every 5 s while the Notebook shows), the known-notes walk |
| `NoteRead { path }` | `Note(NoteText { text, rev })`: UTF-8 (invalid bytes replaced: such a note is shown, `NoteWrite` never replaces it); a file over 4 MiB is refused | opening a note, conflicts, *Mark reviewed* |
| `NoteWrite { path, text, base, roots }` | `NoteSaved { rev }`, or `NoteConflict(NoteText)` with what is there now when the file's revision is not `base` (`None`: the file must not exist yet; a file that is there is a conflict). Atomic (a hidden temporary file renamed over it, tried again for about 0.6 s while another program holds the file), parent folders created, at most 4 MiB; a file gone since its `base` and a file that is not UTF-8 are errors; a link to a note changes its target (inside the roots) | saving (Ctrl+S, autosave), new notes, quick capture, *Mark reviewed*, spec notes, JSON Canvas exports |
| `NoteRevs { paths }` | `NoteRevs(Vec<Option<String>>)`: each file's revision (`None`: missing, unreadable or over 16 MiB); at most 500 paths | change polling of open notes every 2 s |
| `ReadBytes { path, max_bytes }` | `Bytes(Vec<u8>)` (base64 in JSON); a file over `max_bytes` or 16 MiB is an error | pictures in the reader, the GUI's web pane (every file an HTML note loads), the phone's pictures |
| `NoteMkdir { path, roots }` | `Done` (an existing path is an error) | *New folder…* |
| `NoteRename { from, to, roots }` | `Done`; never over another file, also one that appears meanwhile (a change of case only is allowed), never a folder into itself | *Rename…*, *Move to…* |
| `NoteTrash { path, root }` | `Done`: moved to `<root>/.trash/<ms>-<name>` (`<ms>-<n>-<name>` when taken) | *Delete* in the explorer |
| `NoteSearch { roots, query, limit }` | `NoteHits(Vec<Hit { path, line, text }>)`: case-insensitive matches in file names (`line` 0) and in the lines of text files (Markdown, HTML and known text extensions, at most 2 MiB each; the line shortened to 200 characters around the match); `.trash`, `.git`, `node_modules` and links skipped, and the contents of files only in the cloud (OneDrive placeholders: reading would download them; listings classify them by folder); a root inside another searched once; at most `limit` (1 to 500) hits, 20,000 files, 50,000 names a folder, 256 MiB and 10 s | backlinks, `[[` completion, opening a `[[name]]`, the phone's search |
| `SkillSources { dirs, canvas, folders }` | `SkillSources(Vec<SkillSource>)`: each source (`kind`: `Canvas`, `Group { id, name }` (`id` empty: every session's), `Project { root }`, `Personal { agent }`, `Library`), its folder, the agents that load it, the folders read with their counts, any problem, and its skills (name, description, SKILL.md, the agents that load it, its folder, any problem). `dirs`: the canvas's working folders (each one's project is the nearest folder holding `.git`, else itself; at most 200 folders, 40 projects); `folders`: `FolderSet { group, name, dirs }` (at most 20 folders each); `canvas`: its id (the library; an invalid id lists none). Personal sources for the agents installed where the service runs. Read only: SKILL.md front matter from the first 8 KB, at most 200 skills a folder, a few levels deep (docs/AGENT-PROVIDERS.md, *Where agents find skills*) | the ✦ Skills view (on opening it and ⟳), the skill folders editor (counts, and checking a folder before it is added) |

Paths are absolute paths on the service's machine. Every notebook request
refuses relative paths and `.` or `..` components. The reads (`NotebookList`,
`NoteRead`, `NoteRevs`, `ReadBytes`, `NoteSearch`) take any such path, as
`ReadText` does: a client can do what you can on that machine. The writes
(`NoteWrite`, `NoteMkdir`, `NoteRename`, `NoteTrash`) also refuse paths inside a `.git`
folder and paths that are not strictly inside one of `roots` (the client
names the explorer's roots: the notebook root, group folders outside it, the
canvas-wide folder, the mounts; they keep mistakes inside the notebook), on
Windows names Windows would read as other names below the root (a trailing
dot or space, `:` streams, device names such as `nul.md`, `GIT~1`), and
paths a link or junction leads out of every root (checked right before the
change). A revision is the lowercase hex of the first
8 bytes of the SHA-256 of the file's bytes. Each backend process (the
service, a window's in-process backend) makes one write at a time (the
revision check and the write together).

The three remote-access requests are answered by the service itself (they
need its pairing state) and only for clients on its local pipe; TCP clients
and the in-process backend get an error.

Slow requests (CLI runs, scans, worktrees) are made on background threads and
their results come back as `AppEvent`s, as before. Quick ones (a folder
listing, a path check) are made where the answer is needed; locally they cost
what the direct call cost, remotely one round trip. Remote callers wait up to
`Request::timeout()`: 30 s, 2 min for scans and Task Scheduler, 5 min for
agent configuration, 15 min for plugin updates, migrations, worktrees and
forks; `NoteSearch` and `SkillSources` wait up to 60 s (the first bounds itself
to 10 s, the second scans at most 40 projects), so they are not served with
the quick requests (those of 30 s or less, kept apart from slow ones).

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
| `SetField(name, json)` | set a shared top-level field: `recent_folders`, `defaults`, `vscode`, `idle_stop_minutes`, `idle_stop_v2`, `mcp`, `automations_dir`, `doctor_created`, `default_agent`, `dust_after_days`, `subagents_shown` (Settings › Subagents; a session's own choice is its node's `subagents_shown`), `notebook_root` (the notebook's folder; absent: `<home>/notebook`), `notebook_mounts` (extra explorer roots), `notebook_scripts` (HTML notes allowed to run scripts, by path) |

`ClientMsg::DocEdit { key, base, ops_json }` carries the list (`base`: the
version the client had). The service applies the edits of all clients in
arrival order, validates the result (an edit that breaks the document, such as
a duplicate id, is refused: the sender gets `Error` and the whole document
again, at most every 5 s per client and canvas; refusals within those 5 s are
answered once when they are over, never dropped), bumps the version and sends `ServiceMsg::DocPatch { key, version,
origin, ops_json }` to every client that has the document open, including the
sender, in version order (sent under the document lock).

So the last edit wins per field: one window moving a box while another renames
it keeps both; two windows renaming it keep the later name; a delete wins over
concurrent edits of the deleted box.

### Applying patches

The client's `Replica` keeps the service's document (`synced`, at `version`),
that document plus its own edits in flight (`shadow`), and which fields of
which objects are in flight. For each patch:

- its own (`origin` = its connection id) with the targets of the oldest edit
  in flight: applied to `synced`; that edit is confirmed (an own patch that is
  not, an edit a refusal's resync dropped and the service applied after, is
  taken like another window's);
- another window's: applied to `synced`, to `shadow` except fields of its own
  edits in flight, and to the working copy except fields it changed itself
  that the service has not applied yet (in flight or not sent). Those arrive
  at the service later, so they win there too: every client ends up with the
  service's document.
- When nothing is in flight, boxes and groups are put in the service's order.
- A missing version (a patch was lost) makes the client ask for the whole
  document again (`DocOpen` on the same connection).

When another window overwrites a field this client changed in the last 30 s
(a box's name, position, …; not review or last-use times), the client shows "“name” was
changed in another window." The same goes for user-facing settings
(defaults, VS Code settings, MCP presets, idle stop, automations folder,
default agent, the notebook folder and mounts).

Boxes created at the same time by several windows for the same thing
(automation boxes, the Automations group) get ids derived from what they
show (`sync::stable_id`), so they end up as one box.

### Reconnecting

After a reconnect (or a service restart) the client opens the document again.
`Replica::resync` takes the service's document and puts the client's own
changes that are not known to have arrived on top; they go out with the next
edit. While offline nothing is sent; local changes wait. A client that
quits meanwhile (or while it waits for the document again) keeps them next
to its preferences (`<home>/prefs/<client>-<key>.unsent.json`, written by
`App::save_now`); its next start puts them on top of the document and sends
them, then forgets the file. A `DocOpen` whose
`Doc` has not come 10 s later is sent again (then after 20, 40 and 80 s); an
answer that comes after the first one was taken, no newer than this copy, is
dropped (its changes since would otherwise go again, over newer ones). A `Doc`
the client did not ask for is the service's answer to a refused edit:
`Replica::resync_refused` takes it and drops the changes already sent (the
refused one is never sent again), keeping only those not sent yet (S8-05).

### What desktop clients send since round 1 (no wire additions)

| Where | Since | An older service |
|---|---|---|
| `ClientMsg::Claim` from the TUI and the GUI (the terminal that gets the keyboard; a size repair) | the iPhone client added it (every service since, v0.3.0 included, reads it); never sent over protocol 7 | a protocol-8 service from before the iPhone client would drop the connection: not supported |
| `ClientMsg::RefreshSessions` every 10 minutes (every minute in round 1) and when `Stats` disagrees with the client's terminal list (`canvas-core/src/resync.rs`); a whole session list over 64 KiB is sent with the bulk (`outbox.rs`), never ahead of a focused frame | existing message | indexes again (cheap); a service that answers it with `Terminals` (all) and `Sessions { full: true }` to the requester repairs a client's lists at once (older clients take both as the updates they are) |
| A reconnect when `Stats` keeps disagreeing with the client's terminal list (at most every 5 minutes) | — | sends its whole lists, as on any connect |
| A second, short-lived connection for uploads and clipboard images over 256 KiB (`conn::Bulk`: its own client session, no subscriptions, closed when done; since round 2 it sets `CAP_REQUESTS_ONLY` and a service that knows the bit sends it `Ready8` and its answers only: no connect snapshot, broadcast or frame, no input session, not listed) | — | another client for a moment (the bit is ignored: it gets the connect snapshot it throws away); an upload slower than 6 s on it can still be dropped as silent by a service that counts only whole messages, never the window's connection |
| `Request::Upload` pieces of 256 KiB (4 MiB before) | — | takes pieces of any size |
| `ServiceMsg::Error { id: Some(box) }` (a refused launch) shown with its text, the box ↗ when it says the session already runs ("already running", "running elsewhere", "open elsewhere", "open in another", "running in another", "running inside") | — | its texts ("…already running in another terminal…") already match |

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

### The canvas id

When the service opens a canvas without a `canvas_id` (a new one, or one from
before canvas-wide agents), it gives it one (a UUID) and saves it at once.
The id names the canvas's canvas-wide agents' folder,
`<home>/canvas-agents/<canvas id>`, under the service's own data folder. It
is not a shared field: clients read it from `Doc` and never change it (an
older service would refuse a `SetField` it does not know). A canvas without
an id (opened by an older service) shows its canvas-wide agents as
unavailable and does not route them.

### Boxes hidden on their own

A subsession or subagent box hidden from its menu (the canvas's hidden
area, README *Hidden area*) is `Node::hidden: true`. It is an ordinary node
field (it travels as `NodeFields(node, ["hidden"])`, no new `Op` or
`SetField`), so every window and remote client of the canvas sees it. The
hide switches `H` and `J` are each window's own (`UiPrefs::hide_subsessions`,
`UiPrefs::hide_subagents` in its preferences file), never the document's.

| Where | Addition | An older peer |
|---|---|---|
| `Node` | `hidden` (`#[serde(default, skip_serializing_if = "Not::not")]`: not written while false) | an older client ignores it (the box shows there) and keeps it unless it rewrites the whole node; an older service drops it when it applies the edit and saves the canvas (the box shows again everywhere): hiding one box needs a current service |
| `ClientPrefs::ui` (the window's preferences file) | `hide_subagents` (not written while false); `hide_subsessions` keeps its meaning (subsessions, now without a top-level session's subagent boxes) | an older client ignores `hide_subagents`; prefs files from before load unchanged |

## Canvas-wide agents on the wire

All additions are JSON with defaults, so older and newer clients and services
read each other's messages:

| Where | Addition | An older peer |
|---|---|---|
| `Doc` | `canvas_id`; `Passive::scope` (`"canvas"`), `skills`, `groups` (group ids it watches), `disabled`; canvas-wide agents' boxes have `source: "canvas-agent"`, their frame group `source: "canvas-agents"` | ignores them; the box's `source` keeps it a canvas-wide agent for newer clients even if an older one rewrites its passive settings |
| `Doc` | `Passive::model` (WI-028): a canvas-wide agent's own model (`Some("")`: its CLI's default; absent: the default new sessions of its agent get), not written while unset | an older client ignores it and its agent's runs use the default model; an older service drops it when it saves the canvas (the agent falls back to the default model): an agent's own model needs a current service |
| `ClientMsg::Routing` (`bus::Routing`) | `PassiveSpec::model` carries the agent's own model when set (the field existed; nothing new on the wire) | — |
| `ClientMsg::Routing` (`bus::Routing`) | `canvas` (the canvas id); per `PassiveSpec`: `canvas_wide`, `skills`, `groups`; disabled agents are left out | an older service treats canvas-wide agents as passive sessions of their frame (which has no folder: their runs fail visibly) |
| `bus::Event` | `canvas` (set by windows for the events they send, and by the service for events of the routed canvas); `to` on any event: one passive session or canvas-wide agent (`emit --to NAME`), or `canvas` (the canvas-wide agents only) | ignores `canvas`; reads `to` on messages only |
| `ClientMsg::Bus` actions | `{"action": "stop", "key": …}`: ends a passive session's headless run in progress; it ends as stopped (no failure, its events dropped). Handled in the bus loop; an inbox file can never ask for it | ignores unknown actions |
| `ServiceMsg::Bus` (`bus::Snapshot`) | per passive session: `failures`, `retry_at_ms`, `stopped_ms` | ignores them |
| `Request` | `CreateCanvasAgent`, `CanvasSkills`, `CreateCanvasSkill` (see the table above) | answers "does not support this request (update it)", which the panel shows |

The *New canvas agent* dialog and an agent's settings (WI-028) add no
request: the dialog's instructions are `CanvasAgentFiles::rules` of
`CreateCanvasAgent` (unchanged), every other setting is a canvas edit of the
box (`Passive`, `Node::agent`, `Node::model`), and the instructions editor
reads and writes `rules.md` with the notebook's `NoteRead` / `NoteWrite`
(base revision, `roots` = the canvas-agents folder; a change made meanwhile
answers `NoteConflict`, so a second Save is needed). A service older than the
notebook requests answers "does not support": the editor then reads the file
with `ReadText` and says the service must be updated to save it (*rules.md*
still opens the file). The settings row shows the first lines of `rules.md`,
read with `ReadText` in the background (at most every 10 s while its details
show).

The service keeps one routing table: the canvas whose window sent it last.
Canvas-wide agents get the events of that canvas only (`Routing::from_canvas`:
the event names this canvas's id, or its sender is one of this canvas's
sessions, or a window sent it for one of its groups); the bus state of
canvas-wide agents of other canvases is kept (30 days) until their canvas is
routed again. Who may address whom (`Routing::resolve_from`): a session or a
group passive session reaches its own group's sessions and passive sessions
(ungrouped sessions: the other ungrouped ones) and every canvas-wide agent; a
canvas-wide agent and you (a window) reach every session and passive session
of the canvas. Canvas-wide agents can put effects on any box of the canvas
(`effects::resolve_canvas`).

## Skills and skill folders on the wire

The ✦ Skills view asks the backend (`SkillSources`); group skill folders are
launch defaults (docs/AGENT-PROVIDERS.md, *Group skill folders*):

| Where | Addition | An older peer |
|---|---|---|
| `Doc` | `LaunchDefaults::skill_dirs` in `defaults` and a group's `defaults` (not written while empty) | an older client ignores them, but writes `defaults` without them when it changes the canvas's or a group's session defaults (the folders must be set again); an older service drops them when it saves the canvas: skill folders need a current service |
| `ClientMsg::Launch` (`LaunchKind::Agent(AgentLaunch)`, postcard) | none in its shape: the folders and the group's id travel in `AgentLaunch::env` as `ATELIER_GROUP_SKILLS` (JSON `GroupSkills { group, dirs }`), taken out by the service (any case; malformed: dropped, the launch goes on) before the agent starts | an older service passes the variable to the agent (it does nothing there); an older client sends none |
| `ClientMsg::Routing` (`bus::Routing`) | `PassiveSpec::skill_dirs`: a group passive session's folders (the canvas's and its group's) | ignores them: its runs get no skill folders |
| `Request` / `Response` | `SkillSources` / `SkillSources` (appended last) | answers "does not support this request (update it)": the view says the service is older and lists the agents' own lists only; the editor refuses to add a folder |
| `HostApp::resolve_launch` (in-process) | gets the terminal's folder (`cwd`), which tells the service which project skills the agent already loads | — |

The service writes the copies only below its home (`<home>/group-skills/<instance>/<group>/<n>-<terminal>`, a folder for each launch) and removes only there; the skill folders themselves are only read.

## Pull request coordination on the wire

The service owns the routed canvas's pull request registry
(`<home>/canvas-agents/<canvas id>/prs/registry.json`, written atomically by
the bus thread and the PR monitor thread under one lock; `prs_cli::load`
reads it for `pr list`). Everything is JSON with defaults:

| Where | Addition | An older peer |
|---|---|---|
| `Doc` / `ClientMsg::Routing` | `Passive::prs` / `PassiveSpec::prs` (`PrSettings`: `poll_minutes`, `auto_track`, `allow`) on canvas-wide agents | ignores it: the agent is routed without PR coordination, nothing is monitored |
| bus inbox | requests `{"type": "canvas.pr", "source": <session>, "data": {"op": "track" \| "untrack" \| "check" \| "dispatch" \| "dismiss" \| "retry" \| "reassign", "request": <uuid>, "url", "session", "title", "key", "event", "text", "dispatch", "to"}}` (`atelier pr …`); intercepted by the bus loop, never routed to agents | an older service would route it as an ordinary event nobody subscribes to; the command then says the service did not answer |
| `<home>/bus/answers/<request>.json` | `{ok, message, state}`: the service's answer the command waits for (10 s; removed by the command, stale ones after an hour) | — |
| `ClientMsg::Bus` | `{"action": "prs", "op": …}` from windows (the panel's buttons), handled as the same request from you | ignores unknown actions |
| `ServiceMsg::Bus` (`bus::Snapshot`) | `prs`: a `PrsView` (canvas id, the running coordinators, why monitoring is paused, their permissions, the tracked pull requests without watermarks, the last 6 follow-ups each); absent while the canvas tracks nothing and has no coordinator | ignores it |
| bus events | `pr.tracked`, `pr.comment.new`, `pr.checks.failed`, `pr.approved`, `pr.changes_requested`, `pr.merged`, `pr.closed` from the PR monitor, with the owner as `source` and the canvas id set (see AGENT-PROVIDERS.md) | ordinary events |
| mailbox | follow-ups are `Mail` with the dispatch's id (`d-…`), so delivery (one per bus pass, idle owners only) confirms them | — |
| Reports spool | the service spools its own short messages (`reports::notice`): follow-up changes and monitor news in the thread `pr:<key>` | — |

Remote clients get the panel through `ServiceMsg::Bus` and act through
`ClientMsg::Bus`, like the passive sessions; the commands run on the
service's machine (in its sessions), so nothing else crosses the wire.

## Review follow-ups on the wire

The same pattern (`service/reviews.rs`; records in
`<home>/canvas-agents/<canvas id>/reviews/book.json`):

| Where | Addition | An older peer |
|---|---|---|
| `Doc` / `ClientMsg::Routing` | `Passive::reviews` / `PassiveSpec::reviews` (`ReviewSettings`: `channel`, `tool`, `recipients`, `approval`, `check_minutes`, `paused`); *Pause checks* sets `paused` in the document | ignores it |
| bus inbox | requests `{"type": "canvas.review", "data": {"op": "handoff" \| "draft" \| "sent" \| "needs-you" \| "failed" \| "checked" \| "request" \| "done", "request": <uuid>, …}}` (`atelier review …`), answered in `<home>/bus/answers/` | as for `canvas.pr` |
| `ClientMsg::Bus` | `{"action": "reviews", "op": "approve" \| "dismiss-message" \| "assign" \| "dismiss" \| "unwatch" \| "check-now", …}` (the panel's buttons, as you; only you approve, and only from a window: an `approve` found in the bus inbox, where any process can drop a file, is ignored like `canvas.stop_run`) | ignores unknown actions |
| `ServiceMsg::Bus` (`bus::Snapshot`) | `reviews`: a `ReviewsView` (the agents, their settings, why checks are off, the check state, the newest 20 reviews **without private notes** and with shortened results, the conversations, the newest 30 requests) | ignores it |
| bus events | `review.completed` (subscribers), `review.message.approved` and `review.check_due` (addressed to the agent, `to` = its key) | ordinary events |
| mailbox | requests are `Mail` with their id (`q-…`); delivery confirms them | — |

## The notebook and the todo boards on the wire

The notebook's files are read and written only through the backend requests
above (front ends, the phone included, never touch them); the todo book only
through the service's todo hub (`service/todos.rs`). Everything is JSON with
defaults:

| Where | Addition | An older peer |
|---|---|---|
| `Doc` | `notebook_root`, `notebook_mounts`, `notebook_scripts` (shared fields, not written until set) | a client ignores them; a service refuses a `SetField` it does not know (the sender gets `Error` and the whole document again), so changing these settings needs a current service |
| `ClientMsg::Routing` (`bus::Routing`) | `notebook_root` (the canvas setting, trimmed; absent for the default) for `atelier note` and report filing | ignores it |
| `ClientPrefs` | `space` (`notebook` or `tasks`; absent: the canvas) | — (the client's own file) |
| `Request` / `Response` | the ten notebook requests and their answers, appended at the end of the enums; `NotebookList::root` and `NoteWrite::base` may be left out (`None`) | answers "does not support this request (update it)"; the explorer says the notebook is not available |
| `ClientMsg::Bus` | `{"action": "todos", "op": <TodoOp>}` from windows and phones (`todos::client_action`), applied at once (not on the bus loop's next pass) as you; then every client gets the new book. A refusal goes back to that client only, as `ServiceMsg::Error { id: None, message: "Todo change refused: …" }` followed by the book as it is (its optimistic change is undone); an operation the service cannot read is refused the same way | ignores unknown actions |
| bus inbox | requests `{"type": "canvas.todo", "source": <session>, "data": {"op": …, "request": <uuid>, "bot": <automation>}}` (`atelier todo …`), intercepted by the bus loop, never routed to agents, applied as their sender; a session not on the routed canvas is refused | routes it as an ordinary event nobody subscribes to; the command then says the change is handed over |
| `<home>/bus/answers/<request>.json` | `{ok, message, state: "applied" \| "refused", data}`; `data` is the `--format json` answer (`{"schemaVersion": 1, "kind": "todo-change", "ok", "message", "warnings", "id", "created", "changed"}` plus the `todo` or `board` it names). `data` is optional: `pr` and `review` answers leave it out | — |
| `ServiceMsg::Bus` (`bus::Snapshot`) | `todos`: the routed canvas's whole `TodoBook` | absent from older services and while the routed canvas has no id: Tasks shows "Waiting for the todo boards from the service…" |

The bus snapshot is made whole (the book, the PR and review views) and sent
only when something in it changed: each pass compares the snapshot without
the book, and the book's change count (`todos::stamp`), with what every
client got last (`passive::Sent`); a passive session's `next_run_ms` no longer
depends on the clock (it was `now` for the whole 8 s debounce, which re-sent
the whole snapshot every second). A connecting client gets the last snapshot
every client got (kept with `ServiceMsg::Bus`, under the lock it is broadcast
under: nothing is made, no store's lock is waited for, and it never comes
after a newer one); a window whose todo change was refused gets one made
then. Every snapshot still carries the
whole book: an older client reads a snapshot without it as "no book".
Windows' bus actions (`run`, `resume`, `discard_mail`, `pause`) are applied to
the shared state at once and replayed onto the bus loop's copy before it is
written back (`passive::BusOp`): one that arrives while a pass runs is never
overwritten, and mail discarded meanwhile is never typed in (only the messages
there when you discarded them go: one queued later stays).
| `TodoOp` | `edit_check` (`{id, index, text}`: rewords a checklist item, its tick kept; anyone) and `remove_check` (`{id, index}`: people only), appended at the end, sent by the phone | a service of an older build answers "Todo change not understood by the service … restart the service after updating" with the book as it is: the phone's toast says so and its change is undone |
| bus events | `todo.created`, `todo.proposed`, `todo.accepted`, `todo.status`, `todo.claimed`, `todo.submitted`, `todo.done` (data: `id`, `title`, `type`, `priority`, `status`, `board`, `board_name`, `by`, `by_session`, and `from`, `to`, `why`, `resolution`, `pr`, `for`, `for_session` where they apply) in the first canvas group linked to the board (else its parent's), from the actor's session (so never back to it), with the canvas id and the request's hop | ordinary events |
| group catalog | `events.yaml` gains the seven `todo.*` entries (below) | — |

**The hub.** The service holds the routed canvas's book, read from
`<home>/canvas-agents/<canvas id>/todos/book.json` when that canvas is
routed (the one held before is saved first), and is its only writer: after
every applied operation (once the book's lock is released, one writer at a
time, so a slow disk never holds up the bus snapshot, a connecting client or a
launch) it writes the book atomically when its text changed,
rewrites the Markdown mirror (`settings.mirror`: an absolute `.md` path on its
machine; a relative one is refused) when its text changed, and appends to
`log.jsonl` one line per applied or refused operation (`ms`, `actor`, `op`,
`ok`, `message`, `warnings`; an import's text over 2,000 bytes is logged as
its size; moved to `log.jsonl.old` beyond 5 MB). A failed save or mirror
write is a warning in the answer, never a refusal. Every operation runs
through `TodoBook::apply` (all or nothing, refusals in words for the actor)
with the actor (`Actor`: human, agent with its session, or bot with a name),
the actor's group (none for canvas-wide agents), the canvas's groups and
sessions, and the sessions that run here or elsewhere (`live`: a claim by any
other session is stale). Window operations are applied as you with hop 0;
requests with their sender and hop.

**Merged pull requests.** A `pr.merged` event that came from the service
itself (the PR monitor, a session's history), never from an inbox file,
finishes the todos linked to that pull request that are in progress or in
review: `TodoOp::Done` as the bot "pull request merged", the link matched by
the pull request's key.

**Digests.** A group passive run's `group-context.md` gains `# Todos` (the
boards linked to the group: counts, rules, up to 20 open todos, in progress
first); a canvas-wide agent's `canvas-context.md` gains every board in a few
lines.

**The catalog upgrade.** `bus::ensure_catalog` (the `EnsureCatalog` request,
and every run of a group passive session) creates a missing catalog from the
default; an existing one with none of the `todo.*` events gains the seven
default ones (`bus::upgrade_catalog`). Only lines are inserted, after its last
event, in its indentation and line ends, and only when the result reads as
the same catalog plus those events (one YAML document with a top-level
`events:` list of `- name:` entries; any other layout is left alone, and a
catalog with some `todo.*` event is the owner's choice). It is written to a
`.yaml.tmp` file renamed over the catalog.

**Compatibility.** The book is new with this service, so an older service
sends no `todos` and ignores the windows' todo actions and requests. Within
the book, unknown-safe defaults apply (`#[serde(default)]` on every struct);
a service whose `TodoOp::Claim` has no `session` field ignores it and claims
the todo for the requester instead of the session (clients and service come
from one build). Agents get `ATELIER_AGENT` and
`ATELIER_SESSION_AGENT` from the service that launches them
(docs/AGENT-PROVIDERS.md).

## Per-client preferences

The view (position, zoom), panels (sidebar width and visibility, terminal
share, tiled mode, collapsed groups), the recent tile order, the selected
box and the space it shows (`space`: `notebook` or `tasks`; not written for
the canvas) belong to each client. They live in the client's own data folder:
`<home>\prefs\<instance>-<document key>.json`, written 400 ms after a change
and on quit. The first time a client opens a document it starts from the
document's own (legacy) fields. Patches never carry them, so two windows on
one canvas each keep their own view.

## Reports inbox

The service owns the inbox (`canvas_agents::reports::Store`, under
`<home>/reports/`); clients only show it:

| File | Written by | Holds |
|---|---|---|
| `spool/<id>.json` | `atelier report publish` (a `.tmp` file renamed) | a checked `Submission` waiting for the service |
| `spool/<id>.rejected` | the service | why it refused a submission (the waiting command prints it; removed after) |
| `items/<id>/meta.json` + `body.md` / `body.html` / `body.txt` | the service (a `.<id>.tmp` folder renamed) | a stored report |
| `state.json` | the service | the read state and the last 20 refusals |
| `service.json` | the service (at start) | its pid and start time, so the command knows whether to wait |

Ids are `<13-digit ms>-<8 hex>`, so they sort by time. The service's
`reports` thread takes the spool every 250 ms: it validates each submission
again (size, UTF-8, kind, links, thread key), resolves the publisher's box
name and group from routing, stores it or writes its refusal (an inbox
holding 1,000 unread reports refuses new ones; unread reports are never
dropped), keeps read reports for 30 days and at most 500, and broadcasts the
inbox (`ServiceMsg::Reports`, a `reports::Snapshot` as JSON) to every client
with `CAP_REPORTS` when it changed, and to each such client when it connects.

A stored Markdown or HTML report from a canvas session whose group has a
folder is also filed as a note (`reports::file_report`):
`<folder>/human-outputs/reports/<local date>-<slug of the title>.<md|html>`
(`report` when the slug is empty; `-2`, `-3` when taken), with `title`,
`audience: human`, `author: ai`, `agent` (passive sessions' agent),
`source_session`, `created` (the report's time) and `report: <id>`. Messages
and reports from outside the canvas are not filed; a failure is logged
(`report … stored but not filed in its group's folder: …`) and never refuses
the report.

Read state changes are bus requests, like the passive actions:
`{"action": "reports", "op": "read" | "unread" | "read_all" | "delete", "ids":
[…]}` (`service/inbox.rs::action`, checked before the passive actions; an
older service ignores them). They change `state.json` (or delete the item
folder) and broadcast the new inbox, so every window and every remote client
shows the same read state.

## Restore after a restart

`canvas-agents/src/service/restore.rs` (README "Restore after a restart"). The
service keeps a **running set**, `<home>/run/restore-<instance>.json`
(written atomically, owner-only): per terminal its launch as it is to be
started again (an agent's `AgentLaunch` with `resume` on, no first prompt or
fork, the learned `native_id`; a command box's `LaunchKind::Command` from the
open canvas), folder, size, start time and what it was doing (`doing`:
working, waiting, `loop_ms`, from the session summary and the terminal's
progress report). Hooks in `service/mod.rs`: `resolve_launch` notes the
request (passive runs and VM launches are never recorded), the restore
thread confirms it once the terminal runs (and keeps `doing` every second),
`terminal_stopping` outside a shutdown marks your stop, `terminal_exited`
removes the entry on your stop (or a forced stop outside a shutdown), keeps
it while the service shuts down, and otherwise (the program quit on its own)
removes it 10 s later unless the service goes away first (a logoff or reboot
ends programs before the service: exits less than 3 s before a shutdown are
kept). The file is written from a copy taken under the restore's lock,
after the lock is released (launches, client connects, the bus and exits take
it; a flush on a busy disk took seconds), at once for a launch, an exit or a
learned id and at most every 5 s for what sessions do. At start, with
`restore` on (`Settings::restore`, default on), the restore thread relaunches
the entries through `Host::launch` (the same path as a client's launch), two
at a time and 2 s apart, the most recently active first (`Entry::active_ms`;
busy ones first), the next once one has printed and been quiet 2 s (at most
20 s); the snapshot lists every waiting one as `resuming`. It skips entries a
client started meanwhile (`running_or_launching`) or stopped, VM boxes and
refused launches; a session live elsewhere (`Provide::is_live`) is tried
again every 2 s for 30 s, then skipped with its holder (`Provide::holders`:
a canvas box, VS Code, or a program and its pid) as the reason. A client's
`Stop` of a box with nothing running and no launch under way (one waiting
for its turn) reaches the backend as the bus request `{"action": "restore",
"op": "cancel", "id"}` instead of a "no terminal" error (`server::dispatch`,
R2-C-01); one stopped while the restore itself launches it is stopped as
soon as that launch is done (no note, not recorded again), and a cancel that
finds the box just resumed stops it (R3-B-04). A resumed agent that was busy gets its note
once it has run 6 s and been quiet 2 s: a `Mail` with no sender (typed as
written, `bus::mail_text`) put first in the mailbox by the bus loop
(`take_notes`); other mail for it waits until then (`holds`). The bus's
`idle` check treats a history older than the terminal (an agent that writes
nothing until prompted) as idle once the terminal is quiet. Passive runs:
`load_state` queues an interrupted run's events again after an
`atelier.restarted` event with the note.

| Where | Addition | An older peer |
|---|---|---|
| bus snapshot (`ServiceMsg::Bus` JSON) | `restore`: `{started_ms, previous_ms, resuming, resumed, interrupted: [{id, why}]}` (absent when nothing ran before) | an older client ignores it (it shows the history's state: no "resuming after restart") |
| `settings.json` | `restore: false` (absent: on) | an older service ignores it (it restores nothing anyway) |
| `ClientMsg::Bus` | `{"action": "restore", "op": "cancel", "id"}`: a box waiting for its turn that you stopped is not resumed (one removed from the canvas is cancelled by its `DelNode` edit, no request needed) | an older service ignores the action |

`canvas_model::status::Status::{Interrupted, Resuming}` are computed on
clients (not a wire enum); the iPhone maps them to `StatusKind::Stopped` (label
"interrupted") and `Preparing`.

## VM sessions on the wire

VM sessions are optional ([VM-SESSIONS.md](VM-SESSIONS.md)); without a VM pack none of this is sent or shown.

| Where | Addition | An older peer |
|---|---|---|
| `Doc` | `Node::placement` (`{"on": "vm", "repo", "branch", "size", …}`; absent: this machine), `LaunchDefaults::placement` in `defaults` and a group's `defaults` (not written while unset) | an older client ignores them (a VM box looks local there; its double-click is refused by the service: "this session runs on a VM: update this client"); an older service drops them when it saves the canvas |
| `ClientMsg::Launch` (`AgentLaunch`, postcard) | none in its shape: a VM launch names its agent `vm:<kind>` and carries `ATELIER_PLACEMENT` (JSON `VmPlacement`) in `AgentLaunch::env`, taken out by the service | an older service refuses the launch ("unknown agent"): a VM session never starts on the hub by mistake |
| `TodoBook` | `AgentPolicy::start_on_vm` (not written while false) | an older service drops it when it rewrites the book |
| `ServiceMsg::Bus` (`bus::Snapshot`) | `vm`: a `VmView` (the pack's names, every VM box's state, sandboxes, images, budget, queued starts, refusals); absent without a pack | ignores it |
| `Request` / `Response` | `VmList`, `VmPool`, `VmImages`, `VmAction { box, op }`, `VmImageBuild { repo }`, `VmBudget`, `VmPackStatus` / `Vm(VmView)`, `VmPack(PackStatus)` (appended last); answered by the service itself on a thread of its own (`Request::is_service_state`; a delete checks the VM's repositories through its link); an in-process backend refers to it, so local clients send these over their connection (`canvas_core::remote::Routed`) | answers "does not support this request (update it)" |
| `ClientMsg` / `ServiceMsg` | `Sat` (index 29 each), satellites only (below) | never sent to an older peer: satellites authenticate with credentials only a current hub mints |
| `<home>/vm/` | `launches/<box>.json` (VM launches' intents and their steps), `hub.json` (this hub's id, the `atelier.hub` label), `satellites.json` (satellites' credentials, sealed; revoked ones keep only a key to prove the refusal), `sandboxes.json` (role, lease, size, image), `images.json`, `budget.json` (the day's estimated cost), `mirrors/<sandbox>/` (the guests' mirrored history and what was taken: `copilot/<session>/events.jsonl|workspace.yaml`; Claude Code's `claude/projects/<dir>/<session>.jsonl`, its subagents and `claude/sessions/<pid>.json`; moved to the new sandbox's when a conversation resumes on a new VM), `links/<sandbox>.json` (linked or since when, the last load and git report; a delete's check through the link, the satellite's `git` control with the boxes' workspaces, writes it too), `ops/<sandbox>/<id>.json` (each relayed operation's answer, once), `local/` (`vm::local`'s sandboxes and images when the pack names no `local.root`; tests and CI only); each versioned, written atomically by the service only, never written over when unreadable or newer | — |
| handshake | a `Hello8` whose `remote_id` is a satellite's (`sat-` and 16 hex digits) authenticates against `satellites.json`; over TCP only | an older service refuses the id (not a paired client) |
| `launches/<box>.json` | `terminal`: the generation of the box's terminal running in the guest (not written while 0); a restarted service adopts it | an older service ignores it |
| `<home>/vm/links/<sandbox>.json` | the satellite's state as the hub last knew it: linked or not and since when, its last stats (`StatsReport`) | — |

**The satellite's link** (`canvas_agents::vm::satellite` in the guest, `vm::link` on the hub; the messages in
`canvas_common::sat`, versioned on their own: `SAT_PROTOCOL` 1). The guest's `atelier satellite` reads
`~/.atelier/satellite.json` (`hub`, `id`, `secret`, `sandbox`; optional `ca`, `headers`; written by the launcher at
a claim, owner-only), dials `wss://`, `ws://` or `tcp://` (through `HTTPS_PROXY`/`HTTP_PROXY`'s `CONNECT` unless
`NO_PROXY` names the host; `wss://` with rustls, the platform's roots and the pack's CA), runs the handshake with
`CAP_FRAME8 | CAP_SATELLITE | CAP_COMPRESS`, and sends `SatUp::Hello` (JSON `HelloInfo`: sandbox, build, platform,
guest home, the terminals it holds with their output end and last input applied). The hub serves it only when the
credential names that sandbox and `Hello` comes first; then only `Sat` and `Heartbeat` (anything else closes it),
each terminal checked against the sandbox and its generation. The hub answers `SatDown::Welcome` (JSON
`WelcomeInfo`: each terminal's generation and the output offset the hub has); the satellite replays from there
(`SatUp::Output` with `seq` = byte offset) or, when its 4 MiB ring no longer reaches back, sends `SatUp::Snapshot`
(a VT repaint of its screen at an exact offset) and what followed. The hub takes output in order (a repeat is
trimmed, a gap asks `SatDown::Resync`), numbers input per generation (`SatDown::Input`, acknowledged by
`SatUp::InputAck`, sent again after a reconnect, applied once) and starts a box's program with `SatDown::Spawn` once
its launch runs and the satellite is linked (`+NAME` env entries prepend). Terminal I/O goes ahead of everything else
on both ends; both send a heartbeat every 2 s and drop a link silent for 6 s. The thin lane's drain (history the hub
lacks, relayed commands and their answers) travels as `SatDown::Control {"drain": DrainRequest}` /
`SatUp::ControlResult`. A newer link of a sandbox replaces the older one; a refusal proven by the hub (a revoked
credential) ends the satellite and its terminals.

Reports: every 2 s the satellite looks at its terminals' process trees and sends `SatUp::Proc` (JSON `ProcReport`)
when it changed (else every 30 s): per terminal the agent sessions started inside it (a Copilot process holding a
session's in-use lock under the terminal's agent, as `subsession_loop` finds them locally), and the Copilot sessions
alive in the guest. The hub tells clients of each new one once (`ServiceMsg::Subsession`, the box as its parent) and
counts a mirrored session live only as the guest reports it (nothing is live while the link is down). On the
interval `WelcomeInfo::stats_secs` gives (`ATELIER_VM_STATS_SECS`, else 30 s) it sends `SatUp::Stats` (JSON
`StatsReport`: the terminals' CPU time and memory, the guest's memory, load and disk, and per terminal folder git's
branch, ahead, behind, changed, untracked, stashed and unpushed branches); the hub keeps it in
`<home>/vm/links/<sandbox>.json` and the VM view shows it (`VmInfo::git`, `offline_ms`).

Canvas commands from the guest: the guest CLI (`ATELIER_REMOTE=satellite…`) writes each into
`~/.atelier/vm/outbox/`; the satellite pushes them (`SatUp::Op { id, term, json: CliOp }`, at most 16 unanswered,
again after a minute) and the hub runs each once as its box (`atelier <words>` with the box's identity, `vm::thin`;
4 at a time per sandbox, 64 waiting; the answer kept in `<home>/vm/ops/<sandbox>/<id>.json`) and answers
`SatDown::OpResult { id, json: OpAnswer }`, which the satellite hands to the waiting command. The satellite keeps
`~/.atelier/vm/link.json` (`linked`, `since_ms`) for the guest's commands: unlinked, `todo` reads run on the
canvas's last copy and writes answer *Queued* at once. Drains over the link then carry history only
(`DrainRequest::skip_ops`).

Roots (`vm::sync`, `vm::satellite::files`): every 3 s per linked sandbox the hub asks `{"roots_state":{}}` (a
`RootsState`: per root each file and the revision the hub gave it, the first 16 hex digits of its SHA-256), sends
each root's `SatDown::Manifest { root, json: RootManifest }` (an absolute guest `target` inside the guest's home or
`<roots>/<id>`, `writable`, every file and its revision, `folders` to keep), then the files the guest lacks as
`SatDown::File` pieces (1 MiB each; 32 MiB a pass), and `gone` manifests for roots it no longer has. The guest's
edits of a writable root go up as `SatUp::Note { id, root, name, base_rev, bytes }`; the hub writes one only over
the file it started from and answers `SatDown::NoteResult` (JSON `NoteAnswer`: `ok` and the new revision, or a
`conflict`). Roots: `plugin`, `canvas` (routing and the todo book, as a home), `profile`, `group-skills/<box>`,
`notes/<group>` (writable), `instructions/<group>`, `paths/<n>`. A box's program starts after its sandbox's first
full pass (`%ROOTS%` in its command line becomes `HelloInfo::roots`).

Limits (`vm::flood`): each message kind has a size cap (output 36 KiB, a screen 8 MiB, a report 256 KiB, a control's
answer 30 MiB, …; over it the link is closed), a link's bytes are paced at 32 MiB/s past a 64 MiB burst (the hub
stops reading and TCP slows the guest), process reports are taken at most every 0.5 s and stats every second
(extras dropped), a link holds at most 32 MCP streams, and a second `Hello` or a report on another sandbox's
terminal closes it.

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
| `Sat(Box<SatUp>)` (VM sessions' satellites only; [VM-SESSIONS.md](VM-SESSIONS.md); refused from any other connection) | 29 |
| `Received { bytes }` (flow control: what a `CAP_ACK` client read since `Caps`; sent only to a service whose `Caps` said `SERVICE_ACKS`) | 30 |

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
| `Reports { json }` (the reports inbox; only to clients with `CAP_REPORTS`, `1 << 4`, which the TUI and the GUI set) | 27 |
| `Sat(Box<SatDown>)` (only to a satellite: `CAP_SATELLITE`, `1 << 6`) | 29 |
| `Caps { bits }` (what the service does, `SERVICE_*`; sent right after `Ready8`, only to clients with `CAP_ACK`, `1 << 7`) | 30 |

Client capability bits an older service ignores: `CAP_ACK` (`1 << 7`: the
client reads `Caps` and acknowledges what it reads when the service says
`SERVICE_ACKS`; the desktop clients set it over TCP; REMOTE.md, *Flow
control*), `CAP_REQUESTS_ONLY` (`1 << 8`: the connection carries backend
requests only, the desktop clients' upload connection; a service that
honours it says `SERVICE_REQUESTS_ONLY` in `Caps`, never lists it, sends it no
snapshot, broadcast or frame, gives it no input session and serves only its
`Rpc`, `Stats`, heartbeats and acknowledgements). An input session (what a
client session's input was applied, kept across its reconnections, 256 at
most) is forgotten least recently used first, never while its connection is
listed.

`ServiceMsg::shaped` never sends them to protocol-7 clients. The connection's
reader thread routes `RpcReply` to the waiting caller by id; a dropped
connection fails the calls waiting on it. An answer too large for one frame
(32 MiB) becomes an error answer; the connection stays up. `Tails` reads at
most 8 files and 4 MiB in all.

### How the service serves one client

- **Reading.** One reader thread per connection handles what is quick itself:
  terminal input, sizes, subscriptions, heartbeats, acknowledgements,
  `Stats`, and a remote client's backend requests (`Rpc`: parsed and queued
  for the backend's request threads; remote administration is refused to
  remote clients at once). Everything else (`Launch`, `Stop`, `Forget`,
  documents, the bus, todo and notebook operations, a local client's
  requests, routing) goes to the client's **worker** thread
  (`server::serve_messages`), which handles it in the order it came; its
  first job is the backend's snapshot for the new client
  (`HostApp::client_connected`), so reading starts at once and a slow
  snapshot never makes the client look silent. A `Launch` runs on a thread of
  its own (at most 4 per connection): launches of different boxes run side
  by side, and whatever comes for that terminal meanwhile (keys, sizes, a
  stop, a hide) is held behind its launch and handled after it, in order.
  A message about a terminal that has work waiting (a keystroke right after
  its `Launch`), or that another launch is starting (another connection's,
  the restore's), waits behind that work (at most 20 s for another's launch;
  keys refused after a launch that failed give one error, not one per key);
  what is held behind a `Launch` that found another launch of the box under
  way waits for that one too, so a `Stop` stops what it starts (R3-A-07);
  numbered input is still admitted in arrival order. A reconnection of the
  same client session reads at once (subscriptions, frames, heartbeats,
  `Stats`), and its worker's first job is waiting (at most 5 s) for what its
  predecessor read to be handled: until then every message about a terminal
  waits behind it (R2-A-06, R3-A-10). `Health::working` names a worker busy
  with one message, a dropped connection's still draining included (a wedge
  sign after 30 s); `pending_closes` only a dropped connection whose reader
  has not ended. A message this build cannot decode (a newer client) is
  skipped and logged once; the connection stays.
- **Sending.** Each client has an outbox of lanes instead of one queue
  (`canvas-host/src/outbox.rs`): control (in order: `Ready8`, errors, small
  replies, `Stats`, terminal and session state), the focused tile's frame,
  other tiles' frames, then bulk (documents, bus snapshots, the inbox, large
  replies, images, a whole session list over 64 KiB, which deltas pushed
  while it waits join). State is coalesced, never dropped: the newest
  `Terminals` info per terminal (an info of an earlier run of it never
  replaces a newer one), `TerminalGone`, merged `Sessions` (a full list
  replaces what waits), the newest bus snapshot. At most one frame per tile
  waits; the pump makes the next once the writer wrote it. A frame too large
  for one message (over `MAX_FRAME`) goes as several, its rows in halves (the
  first keeps `full`, the others are diffs on top of it), so the tile is
  never left stale; a row too large even alone goes blank (logged once).
  Lanes are bounded
  in messages and bytes: a client that overflows them is dropped and logged
  (`client N fell behind …`); it reconnects and gets everything again. A
  client that acknowledges what it reads (`CAP_ACK`) gets flow control
  (`outbox::Window`): other tiles' frames and bulk only while the bytes it has
  not acknowledged are under a budget that follows the link; a remote client
  that does not gets bulk paced while it types (`outbox::Pace`; REMOTE.md,
  *Flow control*).
- **State is never missed.** A connecting client is listed, and sent the
  terminal list, under one lock: a launch or an exit is in that list or
  reaches it as a broadcast. Every change of a terminal's state (a launch,
  an exit, a title or progress change, `Forget`) is read and queued for every
  client under that lock too, as are the answers to a `Launch` of a running
  terminal and to `RefreshSessions`: a client never gets an older state of a
  terminal after a newer one. The list goes even when it is empty (a restarted
  service with nothing running yet): the client takes its first list as the
  whole of them (a4bf57f services sent none; a client then clears its list
  when their `Stats` count no terminal). The backend sends the session list under the lock
  its index broadcasts under. A `Launch` of a terminal that runs starts nothing
  and answers the requester `Terminals([its info])`; `RefreshSessions` answers
  with every terminal (`Terminals`) and session (`Sessions { full: true }`),
  then wakes the index (older services only woke the index; older clients
  apply both as usual).

### Wedge signs and restart

Every 2 s the service writes `<home>/run/health-<instance>.json` from a thread
of its own (`server::Health`: clients dropped whose connection thread has not
returned, launches under way, clients whose worker is busy with one message,
with when each began, when the frame pump last completed a pass and when it
last recovered from a panic; written to
a temporary file and renamed, so a reader never sees half of it). `service
status` (its `wedged` list) and `doctor` (*Service health*) read it with the
terminal list (`server::wedge_signs`): a terminal alive with no output 30 s
after its launch, a close pending over 5 s, a launch over 30 s, a client's
message handled for over 30 s, a frame pump that had not completed a pass for
10 s (a pass that panics every time does not count, R3-A-08), a pump panic in
the last 30 s, a health file older than 10 s. A panic in a pass of the frame
pump, or in the exit and change loops, is logged (the pump's at most once a
minute) and the loop goes on; the pump asks again for the screens it was
waiting for, and a panic while one client's frames are made skips only that
client's tiles for the pass. A panic while a client's message is handled (on
its worker or a launch's thread) is contained too (R3-A-03): the client gets an
error naming the terminal, a key held behind it is not acknowledged (so it is
sent again after a reconnection), the work held behind a launch is let go and
the launch thread is given back, so the terminal's later messages and the
connection's end are never held.
`service restart [--yes]` stops the service gracefully, waits for its process
to end and starts it through its login entry (Windows: the `Atelier-Service`
task; macOS: `launchctl kickstart` of its LaunchAgent; Linux: `systemctl --user
restart` of its unit), else as `service start` does.

### One process per session

`Host::launch` reserves the terminal id under one lock before anything is
resolved or spawned: while the terminal runs or another launch of it is under
way (two clients resuming one session, a frozen client's launch replayed
beside its reconnection's) a launch does nothing and the caller shows the one
terminal (`terminal X is already being launched; not started twice` in the
log). `Agents::resolve_agent` then claims the agent session itself
(`Host::claim_launch_key`, key `<agent>:<session id>`): another box resuming a
session that runs or is starting is refused ("already running in another
box"), before the agent could write its own lock; Copilot's
`session-state/<id>/inuse.<pid>.lock` with a live process is refused as
before. `ClientMsg::Launch` is unchanged (postcard: no new fields); all of it
is on the service side, and the restart restore relaunches through the same
path. `selftest --remote faults` sends two launches at once from two
connections and counts the processes.

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
  close and lock release, broken files, the canvas id given once and kept.
- `canvas_agents::backend`: canvas-wide agents' files and skills only under
  the backend's own folder (canvas ids, slugs and skill names checked, never
  a client's path); `canvas_agents::service::passive`: addressed events,
  effects of canvas-wide agents, their run folders, state kept for another
  canvas; `canvas_model::bus`: subscribers across groups and canvases, who
  may address whom.
- `tests/backend_contract.rs` starts a real service (`ATELIER_LISTEN=
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
  (`cargo test --test backend_contract -- --ignored`). The contract also
  publishes a report through the command and reads the inbox and its body
  (`Reports`, `ReportContent`) both ways;
  `the_inbox_reaches_clients_that_show_it_and_read_state_is_the_services`
  checks that only `CAP_REPORTS` clients get `ServiceMsg::Reports` and that a
  read by one client reaches the other.
  `skill_sources_answer_the_same_locally_and_remotely` lists a temporary
  repository's project skills, a group skill folder (and a missing one), the
  canvas plugin the service wrote and a canvas library both ways.
- `canvas_model::skills`: each agent's folders, project roots and the folders
  between, scanning (front matter from the first 8 KB, a few levels deep,
  hidden folders skipped, at most 2,000 folders a scan), every source with
  its agents, the 40-project cap, merging the agents' own lists, filtering by
  agent, same names, group plugins (each name once, the project's own left
  out, limits, rebuilt, removed only inside their root; links below the folders
  chosen never followed, full paths only, a skill whose SKILL.md is not copied
  left out); `canvas_model::launch`: the skill folders of every launch in a
  group, their trip through the postcard `ClientMsg::Launch`, the entry taken
  out whatever its case; `canvas_agents::service`: every agent's command line
  with a group plugin, interactive and passive, and an exit handled after a
  relaunch never removing the relaunch's plugin.
- `canvas_agents::reports`: the command's checks (oversized, empty, bad
  links), queued then stored, waiting for stored or refused, read state and
  retention (never an unread report), a full inbox, titles from the body.
  `canvas_model::reports`: validation, cleaning and limits.
  `canvas_core::reports`: threads, opening marks read and launches nothing,
  unread/delete/links through the service; `richtext`: the layout.
  `canvas_model::report_html`: the sanitizer and the page (the desktop's and
  the phone's look). `canvas_mobile::reports`: the phone's inbox (threads,
  its own changes until the service confirms them, what is new) and its pages
  (Markdown and messages rendered, then sanitized).
- `canvas_model::prs`: link parsing (GitHub, GitHub Enterprise, Azure DevOps
  web and REST links), the registry's persistence, ownership and dedupe,
  `gh` / `az` output parsing, diffing against watermarks (baseline, the
  author's own comments, a failure once per head, reviews), end states that
  stop polling, back-off, settings and permissions; `canvas_agents::prs_cli`
  and `service::prs`: requests posted for the service, who a follow-up
  reaches and why it is blocked, which agents monitor, CLI failures sorted
  into unavailable and failing; `canvas_core::prs`: the panel's buttons are
  service requests and start nothing; the `prs-panel` snapshot.
- `canvas_model::reviews`: hand-offs once per review, drafts (the link, no
  private notes, no claimed approval), approval before sending, one message
  per review, requests once per message, association and ambiguity, one
  request at a time per pull request, the hourly schedule and its end,
  settings, views without private notes; `canvas_agents::reviews_cli` and
  `service::reviews`: requests posted for the service, which agent checks;
  `canvas_core::review_followups`: the panel's buttons, pausing is the
  agent's setting; the `review-followups` snapshot.
- `canvas_model::notebook`: front matter read and rewritten (quotes,
  comments, lists, CRLF, unknown keys and order kept), `canvas-` meta tags,
  classification (metadata, folder, file type, default; internal files and
  passive memory), titles, kinds, revisions, paths that must be absolute,
  inside a root and outside `.git`, Markdown with `#note:` / `#todo:` links,
  new notes' provenance. `canvas_model::todos`: ids and prefixes, ranks,
  every operation and the rules for people, agents and bots, claims (live,
  stale, forced, for a session), WIP warnings, events, the board view,
  import, mirror and JSON Canvas export, digests. `canvas_agents::notebook`
  and the contract's `notebook_contract`: every notebook request against a
  temporary notebook, locally and over loopback TCP, with the same answers.
  `canvas_agents::service::todos`: requests applied as their sender and
  answered, window actions, refusals with the book resent, the log, the
  mirror, `pr.merged`, digests. `canvas_agents::{notes_cli, todos_cli}`:
  parsing, defaults, the skills' contracts. `canvas_agents::reports`: filing
  as notes. `canvas_model::bus`: the catalog upgrade. `canvas_core::{spaces,
  notebook, tasks, editor}` and the view snapshots `notebook-*`, `tasks-*`,
  `goto-dialog`, `canvas-spaces-chips`; `canvas_gui::webpane`: the origin's
  path mapping, what each request is answered, links, keys and the overlap
  rule. `canvas_mobile`: the phone's todos and notebook against the mock.
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
- The todo hub holds one canvas's book at a time (the routed one); another
  canvas's book waits on disk until a window opens that canvas. The notebook
  requests trust the roots the client names (a paired client can write
  wherever it can name a root on the service's machine, as it can through a
  terminal anyway).
