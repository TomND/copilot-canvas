# Developer guide

How to work on Atelier: how the pieces run, which files a common
change touches and in what order, how every test layer works, what CI runs,
and the pitfalls that have cost time before. The short version is the root
[AGENTS.md](../AGENTS.md); what the product does is the
[README](../README.md) (its *Features* reference is the specification); the
design of each area is in its topic doc (table at the end). Paths below are
relative to the repository root; `file:function` names a function in a file.

## 1. Orientation

### The layers

| Layer | Crate | What it owns |
|---|---|---|
| L1 terminal host | `canvas-host` | ConPTY sessions, the emulators (`emulator/ghostty.rs`, `emulator/alacritty.rs`), frames, input encoding, the host server loop (`server.rs`: per client a reader for quick messages and an ordered worker for slow ones), each client's outgoing lanes (`outbox.rs`) |
| L2 terminal view | `canvas-termview` | the client-side terminal grid, selection, links |
| Model | `canvas-model` | the document (`doc.rs`), edits and patches (`sync.rs`), the event bus types (`bus.rs`), the backend RPC types (`rpc.rs`), todos, notebook, skills, reports, PRs, reviews: pure and portable |
| L3 backend and service | `canvas-agents` | agent providers (`agents/`), the history index (`index.rs`), the backend handler (`backend.rs`), the service (`service/`: documents, bus loop, passive runs, the PR, review and todo hubs, the reports inbox), the agent-facing CLI pieces (`cli.rs`, `*_cli.rs`, `reports.rs`) |
| Core | `canvas-core` | the app state machine (`app.rs`), every layout in character cells (`ui*.rs`), dialogs, spaces, notebook, tasks, editor; shared by the TUI and the GUI |
| Front ends | `canvas-tui`, `canvas-gui` | the TUI paints the cells in a console; the GUI paints the same cells with wgpu/glyphon and draws the canvas from a model |
| Binary | `src/` | `atelier.exe`: CLI dispatch (`main.rs`), the self-test, parity, golden, hardening and bench drivers, stand-ins (`fake_copilot.rs`, `fake_gh.rs`, `standin.rs`) |

`canvas-core` no longer depends on `canvas-agents`: everything a front end
needs from the machine goes through the backend (`canvas_model::rpc`), and
the canvas document goes through the service (docs/BACKEND.md).

### Processes at run time

- **The client.** `atelier.exe` with no command runs the TUI
  (`src/main.rs:real_main` → `canvas_tui::run`). With `--gui`, or when the
  `client` setting says so (`main.rs:wants_gui`), `main.rs:launch_gui` runs
  `atelier-gui.exe` from the same folder with the same arguments and
  exits with its code. Unless it connects to another machine, the GUI refuses
  to start without `atelier.exe` beside it
  (`canvas-gui/src/main.rs:use_sibling_service`): that file's hash is the
  build id both sides compare.
- **The service**, one per instance and home. A client that finds none
  starts it (`canvas-core/src/conn.rs:open_local` → `spawn_service`) as
  `atelier service --instance N --home H --sessions-dir S`, detached,
  from a per-build copy: `conn.rs:service_executable` copies the build's
  executable to `<home>/bin/atelier-service-<build id>.exe` (build id:
  the package version and the first 8 bytes of the executable's SHA-256,
  `canvas-common/src/paths.rs:build_id`) and removes older copies that are not
  in use. So rebuilding never fights a running service for its file. The
  service exits after `IDLE_EXIT` (10 minutes, `canvas-host/src/server.rs`)
  with no terminals, no clients and no pending work.
- **At service start** (`canvas-agents/src/service/mod.rs`, where the threads
  are spawned) it writes the canvas plugin with the skills to `<home>/plugin`
  (`cli.rs:write_plugin`) and copies itself to `<home>/cli/atelier.exe`
  (`cli.rs:install_cli`), which is on the `PATH` of the sessions it launches.
- **An older service.** A local client compares the service's build and
  protocol with its own (`canvas-tui/src/lib.rs`, `canvas-gui/src/main.rs:
  real_main`): an older service with no live terminals is restarted, one
  with live terminals is restarted only after a question; remote clients
  never restart a service. Details: README *Limitations* (Upgrading).
- **The backend.** Local clients answer backend requests in-process
  (`canvas_agents::backend::LocalBackend`); remote ones send them to the
  service (`canvas_core::remote::RemoteBackend`), which runs the same
  handler (`backend.rs:handle`).

### Instances and homes

`paths::Instance::new` (canvas-common `paths.rs`) decides where everything
lives:

| Input | Default | Effect |
|---|---|---|
| `--instance NAME` | `default` | 1-40 characters of `[A-Za-z0-9_-]`; names the service and its files |
| `--home DIR` | `ATELIER_HOME`, else `~/.atelier` | the data folder (created if missing) |
| `--sessions-dir DIR` | `~/.copilot/session-state` | Copilot's history the index reads |

The service's pipe is `\\.\pipe\atelier-<user>-<instance>-<hash>`,
where the hash is taken from the lower-cased home path
(`Instance::pipe_name`), so two homes never share a service even with the
same instance name. What a home holds:

| Path under `<home>` | What |
|---|---|
| `canvas.json`, `demo-canvas.json` | the canvas document (`--demo` opens the second; `service/docs.rs`) |
| `service-<instance>.log` / `.token` / `.port` | the service's log, its pipe token, the TCP port while it listens for remote clients |
| `settings.json` | machine settings (`atelier config`, `canvas_common::settings`) |
| `prefs/<instance>-<document key>.json` | each client's own view of a canvas (`canvas_core::prefs`) |
| `bin/`, `plugin/`, `cli/` | the per-build service copy, the skills plugin, the CLI for sessions |
| `bus/` | `inbox/` (event and request files), `answers/`, `routing.json`, `state.json` |
| `canvas-agents/<canvas id>/` | canvas-wide agents, `prs/registry.json`, `reviews/book.json`, `todos/book.json` |
| `reports/` | the reports inbox and its `spool/` (`canvas-agents/src/reports.rs`) |
| `notebook/`, `uploads/`, `crash.log` | the default notebook root, remote uploads, the GUI's crash log |

### The demo

`--demo` opens `<home>/demo-canvas.json`, created from
`canvas-core/src/demo.rs:demo_doc`: one group, two sessions, a subagent box
and a subsession, every session a PowerShell shell (no agent, no model).
Demo windows send no routing (`canvas-core/src/passive.rs:sync_routing`
returns early), and passive sessions are left out (no ⟲ chip, the group
menu's passive items disabled). **`--demo` alone
still uses your default instance and home**: it connects to your running
service. For a sandbox add `--instance` and `--home`.

### How a client connects

`conn::open` reaches the local service over the pipe (or TCP for
`--remote`), runs the handshake (`canvas-common/src/handshake.rs`; protocol
8 with `Hello8`, protocol 7 as a fallback) and starts a reader, a writer and
a watchdog thread. The client then sends `ClientMsg::DocOpen` and waits for
`ServiceMsg::Doc` (`conn.rs:open_doc`); from then on its edits go out as
`DocEdit` and patches come back as `DocPatch` (docs/BACKEND.md, *The shared
canvas document*). `--remote loopback` takes the TCP path to the local
service (when none runs it starts one listening on `127.0.0.1:0`;
`conn.rs:open_loopback`).

### A sandboxed instance by hand

The owner's canvas and service run on the development machine. Never point
a test at the default instance or `~/.atelier`. A private instance:

```powershell
$sb = Join-Path $env:TEMP 'cc-dev'
New-Item -ItemType Directory -Force "$sb\home", "$sb\sessions", "$sb\work" | Out-Null
$common = @('--instance', 'dev', '--home', "$sb\home", '--sessions-dir', "$sb\sessions", '--cwd', "$sb\work")
target\release\atelier.exe --gui --demo @common        # or without --gui for the TUI
target\release\atelier.exe service status @common      # JSON: running, build, pid, terminals
target\release\atelier.exe service stop @common        # when done (else it exits after 10 idle minutes)
```

`--demo` keeps the sessions PowerShell shells. Without it, a session you
start runs the real agent CLI with your account and allow-all permissions.
The automated sandboxes go further (`src/selftest.rs:run`,
`src/parity.rs:Sandbox::new`): `COPILOT_HOME`, `XDG_DATA_HOME` and
`CLAUDE_CONFIG_DIR` point into the sandbox, and `ATELIER_COPILOT`,
`ATELIER_OPENCODE` and `ATELIER_CLAUDE` point at copies of
`atelier.exe` that act as a stand-in agent while
`ATELIER_FAKE_COPILOT=1` is set (`src/fake_copilot.rs`). Copy that
environment when a manual check needs agent boxes.

## 2. Code map by task

Each recipe lists the files in the order you touch them. The checklists in
`.claude/skills/` (`canvas-add-feature`, `canvas-wire-compat`) cover the
rules; here is where things are.

### Add a backend request

Anything a front end needs from the service's machine (files, Git, agent
configuration) is a request, so it also works over `--remote`.

1. `crates/canvas-model/src/rpc.rs`: append the variant at the **end** of
   `Request` (new optional fields `#[serde(default)]`), and its answer at the
   end of `Response` if no existing one fits. The compiler then asks for an
   arm in `Request::name`. Decide the lane in `Request::timeout`: up to 30 s
   is a quick request (`is_quick`, served apart from slow CLI runs). A
   request about the service's own remote access goes in `is_remote_admin`.
2. Same file, `impl dyn Backend + '_`: a typed helper (as `skill_sources` or
   `note_search`) that maps the `Response` variant and turns
   `Response::Error` into `Err` (`ask`, `done`, `text`, `texts`, `flag`).
3. `crates/canvas-agents/src/backend.rs:run`: the handler arm. It answers both
   local clients (`LocalBackend`, in-process) and remote ones (the service's
   RPC workers, `service/mod.rs`, the `ClientMsg::Rpc` arm). Paths are the
   service machine's; check anything a client names (canvas ids, slugs,
   roots) as the existing handlers do.
4. The remote path needs nothing more: `RemoteBackend` (`canvas-core/src/
   remote.rs`) sends `ClientMsg::Rpc { id, json }`, the reader thread in
   `conn.rs:establish` routes the `ServiceMsg::RpcReply` back by id. An older
   service answers an unknown request with `rpc::unsupported` ("the service
   does not support this request (update it)"); make the caller show that.
5. Callers in `canvas-core`: slow requests on a background thread (as
   `side.rs:App::run_cli_op` and the other callers do), never blocking the UI
   thread on a CLI run.
6. `crates/canvas-mock/src/lib.rs:rpc` (and `notebook.rs:answer` for notebook
   requests) when the iPhone app uses it; otherwise its catch-all answers
   "the mock service does not support …". The phone's calls are in
   `crates/canvas-mobile/src/client.rs`.
7. Tests: a round trip in `rpc.rs`'s tests that also renames the variant in
   the JSON to prove an older service says so (pattern:
   `skill_source_requests_round_trip_and_old_services_say_so`); a contract
   test in `tests/backend_contract.rs` that asks the in-process backend and a
   real service over loopback TCP and asserts the same answers (`Service::start`,
   `Service::connect`, `LocalBackend::new(service.ctx())`).
8. Docs: the request table in docs/BACKEND.md (*Backend requests*).

### Add a document field

1. `crates/canvas-model/src/doc.rs`: the field on `Node`, `Group` or `Doc`
   with `#[serde(default, skip_serializing_if = "Option::is_none")]` (or
   `Vec::is_empty`, `std::ops::Not::not`, a named default function), so old
   canvases load and unset fields are not written. `Node` has no `Default`:
   the compiler lists every `Node { … }` literal (demo, tests, self-test and
   bench drivers) that needs the field.
2. Node and group fields need no sync code: `sync.rs` diffs objects field by
   field through their JSON (`changed_fields`) and sends `NodeFields` /
   `GroupFields`. A top-level `Doc` field that clients edit is a **shared
   field**: add it to the `shared_fields!(…)` list in `sync.rs` (and to
   `NOTICED_FIELDS` when another window's overwrite deserves a notice). An
   older service refuses an `Op::SetField` name it does not know
   (`set_field` fails, the edit is refused, the sender gets the whole document
   again), so a new shared setting needs a current service; say so in the docs.
3. Fields the service assigns (as `canvas_id`) are not shared fields: clients
   read them and never write them. A window's own view state (position,
   panels, space) belongs in its preferences (`ClientPrefs`,
   `canvas-core/src/prefs.rs`), not in the document.
4. An older service deserializes nodes and groups without the new field and
   writes the canvas without it; an older client ignores it, and drops it
   only when it rewrites that whole object. Record what an older peer does in
   the topic doc's compatibility table.
5. Tests next to the code: old JSON loads with the default and an unset field
   is not written (pattern: `doc.rs`
   `notebook_settings_are_shared_and_absent_from_older_canvases`); `sync.rs`
   tests for shared fields.

### Add a service-owned store and push it to clients

The hub pattern of pull requests, review follow-ups and todos: the service is
the single writer of a JSON file, clients get a view in the bus snapshot,
windows act with bus actions and the CLI with inbox requests.

1. Model (`crates/canvas-model/src/<store>.rs`, as `prs.rs`, `reviews.rs`,
   `todos.rs`): the stored type with a format version (`todos::VERSION`,
   bumped when an older build would drop data on its next write), the
   operations and their rules as pure functions, and the view clients get.
2. Service (`crates/canvas-agents/src/service/<store>.rs`): load and save
   under `<home>/canvas-agents/<canvas id>/…` with atomic writes. Follow
   `service/todos.rs:Hub`: a store that cannot be read, or one written by a
   newer version, is never written over; changes are refused with the reason
   until it reads again. The hub follows the routed canvas (one at a time).
3. Inbox requests from the CLI: `is_request(event)` and `request(…)` called
   from `service/passive.rs:bus_loop` (as `prs::request`, `reviews::request`,
   `todos::request`), answered with `prs_cli::write_answer`.
4. The view: an `Option` field on `canvas_model::bus::Snapshot` with
   `#[serde(default, skip_serializing_if = "Option::is_none")]`, filled in
   **both** places a snapshot is made: `passive.rs:bus_loop` (`view.todos =
   …`) and `passive.rs:snapshot` (the snapshot a client gets on connect). It
   reaches clients as `ServiceMsg::Bus` and lands in `App::bus`
   (`canvas-core/src/passive.rs:on_bus`).
5. Window actions: `ClientMsg::Bus { json }` with `{"action": "<name>", …}`.
   The service's `ClientMsg::Bus` arm (`service/mod.rs`) tries
   `inbox::action` and `todos::action` (applied at once) and passes the rest
   to `passive.rs:action`, whose `match` on `action` turns `prs` and `reviews`
   into requests "as you". Older services ignore unknown actions.
6. The CLI (`crates/canvas-agents/src/<store>_cli.rs`): reads go to the
   stored file; writes call `prs_cli::ask(home, KIND, data)`, which posts an
   inbox event `canvas.<store>` with a request id and waits up to 10 s for
   `<home>/bus/answers/<request>.json` (`None`: the service is not running and
   the request stays queued). Print the answer and fail on a refusal
   (`prs_cli.rs:answered`).
7. Client: a panel (`RightDetail`, below) reading `app.bus.<field>`, buttons
   that send bus actions, and a snapshot test of the panel; the mock
   (`canvas-mock`) if the phone shows it.
8. Docs: a "*… on the wire*" table in docs/BACKEND.md (Where, Addition, An
   older peer), the README section and the skill that teaches the CLI.

### Add a bus event

1. Emit it with a `canvas_model::bus::Event` (`kind` such as `todo.created`,
   `group`, `source` = the acting session, `data`, `canvas`): the service's
   own events go into the bus loop's channel; `atelier emit`
   (`canvas-agents/src/cli.rs:emit`) writes inbox files (`bus::post`).
2. Describe it in `bus::DEFAULT_CATALOG` (the `events.yaml` a group gets; an
   existing catalog is not rewritten, `bus::upgrade_catalog` only adds the
   `todo.*` events) and, for an effect, its `effects:` list.
3. Docs: README *Passive sessions and events* (and the section of the
   feature), docs/AGENT-PROVIDERS.md, and the skill text if agents should
   react to it (below).
4. Trust: events from inbox files can be written by any process running as
   the user. Decisions only the service may take (as `pr.merged` finishing
   todos) must use events from its own channel (`bus_loop` keeps them apart:
   `from_channel`).

### Add a `atelier` command

1. `src/main.rs`: a line in `USAGE` (the `--help` text) and an arm in the
   `match` of `real_main` (`["verb", "sub", rest @ ..] => …`). Options are
   read with `option("name")` / `options("name")` from `args.rest`.
2. **The boolean flag list.** `main.rs:parse` treats every `--word` as an
   option that takes the next argument, except the flags listed in one
   match arm (`"--yes" | "--force" | "--parity" | … | "--with-todos"`). A new
   flag without a value must be added there; otherwise it takes the next word
   as its value (`todo list --new --all` would read `--all` as the value of
   `--new`), or fails with "`--new` needs a value" when it is last. Read it
   with `flag("name")`.
3. The work goes in a module: agent-facing commands in
   `crates/canvas-agents/src/<name>_cli.rs` (`prs_cli`, `reviews_cli`,
   `todos_cli`, `notes_cli`, `factory_cli`), remote administration in
   `src/remote_cli.rs`. Identity comes from the environment the service gives
   sessions (`ATELIER_SESSION`, `ATELIER_HOP`).
4. Docs: README *Commands* and the feature's section; the skill that tells
   agents about it. Sessions run the copy in `<home>/cli`, refreshed when the
   service starts.

### Add or change a canvas skill shipped to agents

Sessions the canvas launches get the plugin in `<home>/plugin`
(`--plugin-dir` for Copilot CLI and Claude Code, its `skills` folder for
OpenCode; docs/AGENT-PROVIDERS.md).

1. The text: constants in `crates/canvas-agents/src/cli.rs` (`EVENTS_SKILL`,
   `PASSIVE_SKILL`, `REPORTS_SKILL`, `PRS_SKILL`, `REVIEW_SKILL`) and
   `todos_cli::SKILL`, `notes_cli::SKILL`. Front matter: `name` and a
   one-line `description` that says "Use when …", with no double quotes and
   no `: ` inside it (a plain YAML value; the tests check it for two skills).
2. A new skill: one more entry in `cli.rs:write_plugin`'s `files` array
   (`skills/<name>/SKILL.md`; the array's length is in its type).
3. The plugin version: the same string in `write_plugin`'s `plugin.json` and
   in `CLAUDE_PLUGIN`, and where the docs name it (README *Passive sessions
   and events*, docs/AGENT-PROVIDERS.md). It went to 1.2.0 with `canvas-prs`,
   to 1.3.0 with `canvas-todos` and `canvas-notes`, to 1.4.0 with the rename to Atelier, and to
   1.5.0 with the lines on VM sessions.
4. Tests: `cli.rs` test `effects_are_checked_and_posted_for_the_service`
   writes the plugin and checks each skill's contract (the commands and rules
   it must mention, and which skills point at which). Add the new skill's
   lines there.
5. Delivery: the service writes the plugin when it starts. A running service
   of the previous build keeps serving the old texts until it restarts on
   the new build (`atelier service stop`, then open the canvas again,
   or the toolbar's *Update service*); sessions get the plugin at launch.

### Restore after a restart

`crates/canvas-agents/src/service/restore.rs` keeps the running set and
resumes it at start (docs/BACKEND.md "Restore after a restart"); its hooks
are one line each in `service/mod.rs` (`resolve_launch`,
`terminal_stopping`, `terminal_exited`, `shutdown`) and `passive.rs` (the
note into the mailbox, the snapshot's `restore`). The interrupted and
resuming states: `canvas_model::status` (rules), `App::node_status`,
`App::loop_interrupted` and `interruption_text` (canvas-core),
`draw_broken_loop` (TUI) and `broken_arc` (GUI). Tests: the unit tests in
`restore.rs`, the `interrupted-after-restart` view snapshot, and
`tests/restore.rs` (a killed service resumes a stand-in Copilot session with
its note and leaves the one you stopped alone). The restore file is written
after the restore's lock is released (`Restore::write`; never add file I/O
under `inner`: launches, connects, the bus and exits take it), and entries
resume through `Queue` (two at a time, the most recently active first). The
bus snapshot is broadcast only when it changed (`passive::send_snapshot`;
`tests/bus_quiet.rs` counts them while an event waits out its debounce):
keep clock-derived values out of `bus::Snapshot`.

### VM sessions

`crates/canvas-agents/src/vm/` (docs/VM-SESSIONS.md, the plan in
docs/VM-SESSIONS-PLAN.md, progress in docs/VM-STATUS.md): `pack.rs` (the VM
pack and `vm pack check`), `hooks.rs` (the pack's hooks), `launch.rs` (VM
launches' intents, `vm attach`, the agent's command line in the guest),
`lite.rs` (the guest side of the thin lane: relayed commands, the drain),
`thin.rs` (the hub side: drains, mirrors, running relayed operations),
`provider.rs` (the provider trait and its contract: guest paths from the
guest's home, file modes, bounded reads), `local.rs` (`vm::local` sandboxes
and `vm-hook-standin`), `aca/` (`vm::aca`, the sandboxes' REST API, tested
against an in-process stand-in only), `store.rs`, `manager.rs`,
`launcher.rs`, `guest.rs` (the guest-side `satellite --write-file|--clone|
--prepare`), `secrets.rs`, `spike.rs`, `tls.rs` (the satellite link's TLS),
`satellite/` (the guest's `atelier satellite`: `dial.rs` reaches the hub,
`terms.rs` runs the boxes' PTYs, `ring.rs` keeps their output for replays),
`link.rs` (the hub's side: one link per sandbox, the boxes' remote
terminals, adoption after a restart), `session_store.rs` (a VM session's
history in its mirror); `open_provider` picks the pack's provider. The
canvas-host side is `term.rs`'s `RemotePty`/`spawn_remote` and
`server.rs`'s `serve_satellite`/`adopt_remote`. The model is
`canvas_model::vm`, the satellite link's messages `canvas_common::sat`, the
client side `canvas-core/src/vms.rs` and the new-session dialog's *Run on*.
Everything is off without a pack. `atelier selftest vm` (`src/vm_probe.rs`)
runs it end to end on local sandboxes, twice: the hooks lane, then the pool
lane with `provider: local` (the satellite over `ws://` on loopback, a
restart, a freeze); never point it, or anything else, at a real provider in
a test. Its hub commands pass the probe's `--instance`: a command without it
reaches instance `default` of that home and starts one.

### Agent providers

`crates/canvas-agents/src/agents/mod.rs`: `trait Provider` (`program`,
`command`, `reserved_args`, `history`, `is_live`, `has_history`,
`terminal_exited`, …) with one implementation per agent: `copilot.rs`,
`opencode.rs` (plus `opencode_server.rs`), `claude.rs`; `AgentKind::provider()`
picks it. The service turns a `LaunchKind::Agent(AgentLaunch)` into a command
line in `service/mod.rs` (`HostApp::resolve_launch` → `Provider::command`)
with an `Integration` (`Agents::integration`: the plugin folder, extra skill
plugins, and the environment `ATELIER_HOME`, `ATELIER_SESSION`,
`PATH`). Rules every provider follows: the program comes from
`find_program` (an environment override such as `ATELIER_OPENCODE`,
then `PATH`) and is never a batch script (`ensure_not_batch`); free text goes
through `positional` and option values through `option_value`, so a prompt
or a name never becomes an option. History: Copilot `index.rs` (tails
`events.jsonl`), OpenCode `opencode_history.rs` (its database), Claude Code
`claude_history.rs`; transcripts `agents/transcript.rs`. What the client
sends is built in `canvas-model/src/launch.rs:agent_launch`; `tests/
model_launch.rs` follows the model from a box to each command line. The
behaviour of each agent is docs/AGENT-PROVIDERS.md; `selftest agent` is the
only check against real CLIs (only when asked).

### Add a sidebar view or list rows

- The activity bar is `side.rs:VIEWS` (`SideView`, icon, title), drawn by
  `ui_side.rs:draw_activity_bar`; `Alt+1`…`Alt+9` index it
  (`events.rs:on_key`), so a tenth view needs a key decision.
- Rows of every view but Sessions come from `side.rs:App::build_list`
  (a `match` on `side_view`; some views build theirs in their module, as
  `report_rows` and the skills view): `ListRow { item: ItemRef, label, right,
  icon, color }`, with `ListRow::header` / `info` for rows that cannot be
  selected. A new kind of row is an `ItemRef` variant. Enter and clicks reach
  `list_activate`, Space `list_toggle`, Del `list_delete`; a row that shows
  details sets `right_detail = Some(RightDetail::Item(item))`.
- `ui_side.rs:draw_list` draws the rows; `view_title` and `hints` give the
  title and the key hints.
- The Sessions tree is different: `ui.rs:build_side_rows` makes `SideRow`s
  (`SideKind`: groups, folders, sessions, subagents, passive rows, RECENT), cached and
  rebuilt by `refresh_side_rows` only when it changes.

### Add a right-pane panel

`side.rs:RightDetail` names what the right pane shows when no terminal is
there. `ui_side.rs:draw_right_detail` matches it: most panels are a function
that fills lines and `(label, Action)` buttons (`prs_panel`,
`group_passives_panel`, …); `buttons` registers each as
`Hit::Toolbar(action)`, so a click runs the action through `App::run`. The
Notebook and Tasks draw their own (`RightDetail::Note`, `RightDetail::Todo`:
`notebook::draw_detail`, `tasks::draw_detail`). A panel only shows: its
buttons never start a session except an explicit Start / Resume.

### Add a dialog

1. `dialogs.rs`: the dialog's state and a `Modal` variant (dialogs are UI
   state, never on the wire). Area modules keep theirs behind one variant
   (`Modal::Notebook`, `Modal::Tasks`, `Modal::SkillFolders`).
2. `ui_modals.rs:draw_modal`: an arm calling its draw function. Use
   `centered`, `panel`, `field` and `button`; click targets go through
   `Ctx::hit(rect, Hit::…)`, which keeps only the top-most dialog's
   (`draw_modals` drops the main screen's targets while a dialog is open).
3. `modals.rs`: keys in `App::modal_key`, mouse in `modal_mouse` (the `Hit`
   under the pointer), pastes in `modal_paste`. A dialog's handler yields an
   `Outcome`: `Keep`, `Close`, `CloseAndRun(Action)` or `KeepAndRun(Action)`.
4. Open it from an action (`app.modals.push(Modal::…)` in `actions.rs:App::run`).
5. A view snapshot of it open (the overlay is part of the snapshot).

### Add an action and its keys

1. `dialogs.rs:Action`: the variant (a feature with many gets its own enum
   behind one variant, as `Action::Prs(PrAction)`).
2. `actions.rs:App::run`: the arm (or a call into the feature's module).
3. Bind it: a menu item (`actions.rs`: `node_menu`, `group_menu`,
   `canvas_menu`, `tile_menu`, built with `item(label, hint, action,
   enabled)`), a button (`Hit::Toolbar(action)`), and a key in
   `events.rs:App::on_key`. Add the key to the help (`ui_modals.rs`:
   `HELP`, `NOTEBOOK_HELP`, `TASKS_HELP`, `SPACES_HELP`) and README *Controls*.

`App::on_key` routes a key in this order, and the first taker wins:

| # | Who | Keys |
|---|---|---|
| 1 | an open dialog (`modal_key`) | every key |
| 2 | recent sessions (`recent_key`) | Ctrl+F6, Ctrl+Shift+F6 (also from a terminal), `]` `[` |
| 3 | a terminal with the keyboard (`terminal_key`) | everything: F6 and Ctrl+] leave it; Ctrl+C copies a selection, Ctrl+V pastes, Shift+PgUp/PgDn scroll; the rest goes to the program |
| 4 | spaces (`spaces.rs:space_key`) | Ctrl+1/2/3, Alt+← / Alt+→, Alt+, / Alt+., Ctrl+P |
| 5 | quick capture | Alt+N |
| 6 | activity bar | Alt+1 … Alt+9 |
| 7 | the Notebook or Tasks module (`space_has_keys`) | its main area and sidebar |
| 8 | a list view of the sidebar | arrows, Enter, Space, Del, A, Esc, Tab |
| 9 | Ctrl+Q, the sidebar filter, then single keys | q, F6, ?, /, b, n, s, i, v, a, Ctrl+Z/Y, t, o, f, g, k, h, j, r, x, + -, Del, Esc |
| 10 | the Sessions sidebar, then the canvas | arrows, Enter (the explicit start), Tab, Home / c |

So a new global key goes in row 9 and never reaches a terminal; a key that
must work from a terminal (rare) needs a place before row 3. The GUI takes a
few keys before the core: `canvas-gui/src/shell.rs:gui_key` (Ctrl+Shift+N,
the paste keys outside terminals) and its mouse back and forward buttons.

### Change the canvas drawing

`ui.rs:render` lays out the screen (toolbar, activity bar, sidebar, main
area, right pane, status bar) and draws everything into a cell buffer. For the
canvas it branches on `app.render_mode`:

- `RenderMode::Cells` (the TUI): `ui_canvas.rs:draw_canvas` draws characters.
- `RenderMode::Gpu` (the GUI): `ui_canvas.rs:canvas_model` returns a
  `view::CanvasModel` in world units (boxes, decks, edges, groups with chips,
  loops, effects, the working glow, the halo), which
  `canvas-gui/src/canvas.rs:paint` draws at any scale (with its own layers:
  `paint_glow`, `paint_loops`).

Both build the same `view::BoxModel` per box (`ui_canvas.rs:box_model`,
`deck_model`), and `paint_box` / `paint_model` turn a model into characters
(the TUI, the GUI's `screen` hook and the snapshots use them). A new visual
property is therefore: a field on the model in `view.rs`, filled in
`box_model` (or `canvas_model`), drawn in `paint_box` for the TUI and in
`canvas-gui/src/canvas.rs` for the GUI, shown by the dumps in `view.rs`
(`View::dump`, which the snapshots record), and in the `tree` hook
(`canvas-gui/src/hooks.rs:tree`) when a parity row must see it. The chrome
around the canvas (sidebar, panels, dialogs) is cells in both clients; the
GUI draws those cells crisply (`canvas-gui/src/chrome.rs`).

### Add a group chip or a group menu item

- Chips: `folders.rs:App::group_chips` returns `Chip { label, action, lit,
  attention }` (full and compact labels); `fit_group_chips` decides which fit
  a title row by `Chip::rank`. A chip's click runs its `Action`. The canvas
  model carries chips to the GUI (`view::ChipModel`), the TUI draws them in
  cells; both must stay clickable at every zoom.
- The menu: `actions.rs:App::group_menu` (the canvas-wide agents' frame has
  its own, `canvas_wide.rs:frame_menu`). Items that need the service's bus
  are disabled in demo mode (`!self.demo`).
- The group's details panel and the sidebar's folder row offer the same
  links (`ui_side.rs`, `folders.rs:group_folders_menu`).
- Parity rows open the parity group's menu with the shared `group_menu`
  helper (`src/parity.rs`), which right-clicks a free cell of the title row:
  a new chip moves what is free.

### The Notebook and Tasks spaces

`spaces.rs`: `Space { Canvas, Notebook, Tasks }`, the navigation `History`,
`App::navigate` (the one way to jump between places), Go to (`GoTo`,
`goto_items`) and `space_key`. The two modules, `notebook/` and `tasks/`,
expose the same hooks from their `mod.rs`: `run(app, Action)` (from
`Action::Notebook` / `Action::Tasks`), `click(app, Hit, mouse)` (from
`Hit::Notebook` / `Hit::Tasks`), `open(app, NavTarget)`, `draw_side`,
`draw_main`, `draw_info`, `draw_detail`, `draw_dialog` / `dialog_key` /
`dialog_mouse` / `dialog_text`, `on_key`, `on_mouse`, `on_text`, `tick`,
`goto_items`, `current_target`. `ui.rs:render` calls the drawing hooks for
the space shown (the GUI draws these spaces from cells too: there is no
canvas model there); `events.rs` hands them keys and mouse while
`space_has_keys`. The notebook's files go through the backend requests
(`Note*`), the todo book through the service's hub (above). Design and
behaviour: docs/NOTEBOOK-TASKS.md.

### The GPU client

| File | What it does |
|---|---|
| `canvas-gui/src/main.rs` | arguments (`--test-hooks`, `--size COLSxROWS`, …), connecting, the older-service questions |
| `shell.rs` | the windows and the event loop: each window has its own `App`; frames only when something changed; damage frames that repaint only the tiles; the loop and glow layers; `gui_key`; the hooks (`Shell::on_hook` for process-wide commands, the window's `hook` for the rest) |
| `chrome.rs` | the core's cell buffer (toolbar, sidebar, panels, dialogs) as quads, crisp box drawing and text runs |
| `canvas.rs`, `effects.rs`, `tiles.rs`, `images.rs` | the canvas model, effects, terminal tiles, image textures |
| `webview.rs`, `webpane.rs` | the WebView2 view for HTML reports and notes; `webpane.rs` holds its decisions as plain functions with tests (`webpane/tests.rs`), `webview.rs` wires them to WebView2 |
| `hooks.rs` | the test-hook pipe and the `screen` and `tree` answers |
| `access.rs` | the UI Automation tree (AccessKit), built from the same view; the `tree` hook's `access` |
| `platform.rs`, `placement.rs`, `crashlog.rs`, `perf.rs` | taskbar, toasts, clipboard; window placement; `<home>\crash.log`; frame statistics |

A GUI-only behaviour gets a row in docs/PARITY.md (*New in the GUI*) and its
description in docs/GUI.md; a new hook command goes in `shell.rs`, the
comment at the top of `hooks.rs` and the table in docs/GUI.md §3.

## 3. Testing

| Layer | Where | Run | Opens windows |
|---|---|---|---|
| Unit tests | `#[cfg(test)] mod tests` next to the code | `cargo test --workspace` (about 3 minutes after a build) | no |
| View snapshots | `crates/canvas-core/tests/view_snapshots.rs` | part of `cargo test` | no |
| Backend contract | `tests/backend_contract.rs` | part of `cargo test` (starts real services in temporary homes) | no |
| Self-test | `src/selftest.rs`, `src/selftest_spaces.rs` | `selftest` (TUI, hidden pseudoconsole), `--remote loopback`; `--client gui` | GUI: yes |
| Remote probes | `src/remote_probe.rs`, `src/pair_probe.rs`, `src/soak.rs` | `selftest --remote faults`, `paired`, `soak` | soak with `--client gui` |
| GUI parity | `src/parity.rs`, `src/parity_*.rs` | `selftest --client gui --parity` | yes |
| Golden screenshots | `src/golden.rs`, `tests/golden/gui/*.png` | `selftest --client gui --golden [--update]` | yes |
| Hardening | `src/hardening.rs` | `selftest --client gui --hardening` | yes |
| Terminal corpus | `corpus/`, `crates/canvas-corpus` | `corpus check` (and `cargo test`) | no |
| Benches | `src/bench.rs`, `src/gui_bench.rs`, ignored probes in `src/selftest.rs` | `bench …`, `bench --client gui …` | GUI bench: yes |

The commands are `target\release\atelier.exe …` after `cargo build
--release`. Typical times on the development machine: each self-test about
145 s, parity about 270 s, golden about 30 s, hardening about 32 s, a cold
build about 10 minutes. Suites that open windows run on the owner's desktop:
one at a time, never two together.

### Unit tests

Next to the code, in every crate. Run a subset with a filter:

```powershell
cargo test -p canvas-model sync::                          # one crate, one module
cargo test -p canvas-agents service::todos
cargo test -p canvas-core --lib undo                       # canvas-core's unit tests only
cargo test -p atelier --bin atelier golden   # tests inside src/*.rs
cargo test -p canvas-model -- --nocapture                  # with the tests' output
```

`cargo test` without `--workspace` runs only the default members (the root
package and `canvas-gui`, `Cargo.toml` `default-members`). Some probes are
`#[ignore]`d and run by hand: `cargo test --release stress_probe -- --ignored
--nocapture` (`STRESS_SESSIONS`, `STRESS_AGENTS` scale it), `perf_probe`
(with `PERF_CANVAS=<copy of a canvas.json>`) and `two_clients_probe`, all in
`src/selftest.rs`, and `cargo test --test backend_contract -- --ignored` (needs
an installed agent). Tests use temporary homes (`std::env::temp_dir()` and a
UUID), never `~/.atelier`.

### View snapshots

`crates/canvas-core/tests/view_snapshots.rs` builds app states in-process
(`app(doc, demo)`: a detached connection, the `Offline` backend, fixed paths
`C:\work\canvas`, tick 0, motion on) and renders each at 150 × 40 cells in
both modes: `check(name, &mut app, BOTH)` records the TUI screen
(`RenderMode::Cells`), the GPU view model (`View::dump`) and the dialog
overlay; `Sections { cells: true, painted: true }` adds the GPU chrome with
the model painted as characters (`cells: false` for scales the TUI does not
have). The facility is `tests/snapshot/mod.rs`; the files are
`crates/canvas-core/tests/snapshots/<name>.snap`.

- A mismatch writes `<name>.snap.new` beside the snapshot and fails with a
  line diff (a missing snapshot fails the same way, "no snapshot … yet").
  `.snap.new` files are ignored by Git.
- Run one: `cargo test -p canvas-core --test view_snapshots every_status`.
- Accept: `$env:UPDATE_SNAPSHOTS = '1'; cargo test -p canvas-core --test
  view_snapshots; Remove-Item Env:UPDATE_SNAPSHOTS`, then read every changed
  `.snap` with `git diff -- crates/canvas-core/tests/snapshots` and explain
  each change in the commit. A change you cannot explain is a bug.
- Inputs must not depend on the clock or the machine: fixed ids, paths and
  times, `app.tick = 0`.

### Backend contract tests

`tests/backend_contract.rs` starts the package's own binary as a service in a
temporary home with `ATELIER_LISTEN=127.0.0.1:0` (`Service::start`),
connects over loopback TCP (`Service::connect`, a `RemoteBackend`) and runs
the same function against `LocalBackend::new(service.ctx())` and against the
service; both lists of answers must be equal
(`local_and_remote_backends_answer_the_same`). There are suites for the
notebook (`notebook_contract`), skill sources, the reports inbox and two
clients editing one canvas. A new request gets a line in `contract` (or a
test of its own that compares both answers). Run one:
`cargo test -p atelier --test backend_contract skill_sources`.

### The self-test

`atelier selftest [--client tui|gui] [--remote loopback]` drives the
real client end to end through one scenario, `src/selftest.rs:scenario`,
generic over the `Ui` trait (`send` bytes, `paste`, `clipboard`, `text`,
`wait`, `click`, `double_click`, `drag`, `find`, `locate`, `wait_exit`):

- **TUI**: `Console` runs `atelier.exe` in a hidden pseudoconsole
  (portable-pty) and feeds its output to an alacritty emulator whose screen
  is the text the checks read. Input is what a terminal sends: keys, text,
  SGR mouse reports; `ctrl_digit` and `ESC_KEY` give Ctrl+digit and Esc in
  win32-input-mode.
- **GUI**: `GuiDriver` starts `atelier-gui.exe --test-hooks --size
  170x46`, reads the pipe name the GUI prints, and turns the same input bytes
  into hook commands at cell centres; `text` is the `screen` hook.
- **The sandbox** (`selftest.rs:run`): `%TEMP%\atelier-selftest-<uuid>`
  with `home`, `sessions` and `work`, instance `selftest<pid>`, the client
  started with `--demo`, a sample `COPILOT_HOME` automation database, and
  stand-in `copilot`, `opencode`, `claude`, `gh` and `az` (copies of
  `atelier.exe`, `ATELIER_FAKE_COPILOT=1`). It saves and restores
  your clipboard text, stops its service at the end, deletes the sandbox on
  success and prints `artifacts kept in <root>` on failure (the GUI's stderr
  in `gui-stderr.log`, the service log in `home\service-selftest<pid>.log`).
- **Steps**: `step("what it checks")` prints `  · what it checks`; the
  scenario runs its own steps, then helpers (`view_and_start_steps`,
  `recent_steps`, `group_click_steps`, `report_steps`, `layout_undo_steps`,
  `area_selection_steps`, `selftest_spaces.rs:steps`), quits, and runs the
  steps that need canvases of their own (`canvas_agent_steps`, `pr_steps`,
  `review_steps`, `todo_cli_steps`) and, without `--remote`, an instance of
  its own (`legacy_service_steps`: a stand-in service of the build before
  the rename on its old endpoint is found and replaced). It ends with `PASS in … s`.
- **Adding a step**: write it against `U: Ui` only (no hook commands), so it
  runs in the TUI, the GUI and both over loopback; wait for screen text
  rather than sleeping; then update the step count, now 46 (45 with
  `--remote`, which skips the upgrade from the build before the rename), where the docs
  give it (README *Commands*, docs/GUI.md §3, docs/PARITY.md, the comment
  above `scenario`).
- `CANVAS_GUI_SCREENS=DIR` saves PNGs at some steps (GUI);
  `CANVAS_SELFTEST_DEBUG=1` prints diagnostics at the start.

Remote probes: `--remote faults` (`remote_probe.rs`: drops, cut frames and
delay while typing; exactly-once input; a throttled link with large replies,
a slow upload, and state repair), `--remote paired`
(`pair_probe.rs`: pairing, a Dev Tunnel stand-in, revocation, wrong secrets),
`--remote soak [--minutes N] [--drop-every S] [--client tui|gui]`
(`soak.rs`). `selftest agent` drives the real agent CLIs; run it only when
asked.

### GUI parity

`atelier selftest --client gui --parity` runs one named check per row
of the parity matrix (docs/PARITY.md).

- **The sandbox** (`parity.rs:Sandbox::new`):
  `%TEMP%\atelier-parity-<uuid>` with a home, a work folder, a group
  folder, a `COPILOT_HOME` with the sample automation database, synthetic
  Copilot histories for every status (working, needs you, done · review,
  idle, stopped, failed, open elsewhere, a loop, a subagent), an OpenCode
  database, stand-ins for every agent CLI and for `gh` / `az` (fixtures in
  `prs/`, writes refused), instance `parity<pid>`, and `remote.listen` on
  `127.0.0.1:0` for the rows that connect over loopback. `Sandbox::start`
  launches the GUI with that environment and `--size 170x46`.
- **Order**: `suite` starts one GUI for `main_rows` (the rows in `parity.rs`
  and the modules called with the shared GUI: `parity_reports.rs`,
  `parity_agents.rs`, `parity_prs.rs`, `parity_spaces.rs`,
  `parity_skills.rs`), quits it, then runs `late_rows`: rows that restart the
  GUI or use other clients, and the modules with canvases of their own
  (`parity_visual.rs`, `parity_layout.rs`, `parity_subagents.rs`,
  `parity_pulse.rs`). The table is sorted by row number.
- **A row**: `check(rows, "52", "<feature>", "<test name>", || body)`;
  `check_kind(…, Kind::Only("<why>"), …)` for a row that only means
  something in one front end. The body returns `Ok(detail)` (printed after
  the time) or an error (its first 30 lines printed, its first line in the
  table). The run ends `PASS: N rows in T s` or fails with
  `artifacts kept in <root>` (GUI logs `gui-parity.log` and others there,
  the service log in `home\`). PNG captures go to `CANVAS_GUI_SCREENS` or
  `%TEMP%\atelier-parity-screens`.
- **A subset**: `$env:CANVAS_PARITY_ROWS = '41,41b'` (row ids, any case)
  runs only those rows; the others are neither run nor listed. Rows build
  on earlier ones (39b reads the model row 38 remembered), so add the rows a
  row needs. The full suite is what counts.

**Writing a robust row.** Rows share one GUI and one sandbox, so a row must
not depend on luck and must leave things as it found them:

- Wait for a condition, never a fixed sleep in its place: `gui.wait(what,
  secs, |screen| …)` on the screen text, `gui.wait_tree` on the automation
  tree (boxes with status, border, deck, chips; tiles; focus; modals),
  `wait_doc(&sb.canvas, what, |doc| …)` on what the service saved. The
  error then says what was awaited and shows the screen.
- Find things by what the user sees: `gui.find("<exact UI text>")`,
  `gui.box_center(name)` (a visible point of a box), the `tree` rects. The
  texts are canvas-core's strings, so renaming a label means searching
  `src/parity*.rs`, `src/selftest*.rs`, `src/golden.rs` and
  `src/hardening.rs` for it.
- Use the shared helpers in `parity.rs`: `Gui::canvas_focus` (closes
  dialogs, leaves a terminal, focuses the canvas), `Gui::open` (double-click
  a box and wait for its terminal), `group_menu`, `model_dialog`,
  `pick_model`, `create_from_dialog`, `stop_box`, `inspect_in_deck`; `f`
  (show all nodes) when a box may be out of view.
- "Viewing never starts anything" is checked by counting `launched ` lines
  in the service log before and after (`launches(sb)` in several row files).
- Clean up even when the check fails: compute the result, run the cleanup,
  then return the result first (`parity_skills.rs:skills`;
  `parity_prs.rs:after` with `tidy`). Earlier rows can leave boxes over a
  group's title, a selection, an open panel or a scrolled view; that is why
  `group_menu` looks for a free cell.
- Docs: the row in docs/PARITY.md (the matrix `| # | Feature | GUI check |
  Also | ✓ |`, *New in the GUI* for G rows, *Canvas visuals* for V rows) and
  the *Last run* section (what changed, `PASS: N rows in T s` with the date,
  the self-test, golden and hardening results of the same tree).

### Golden screenshots

`atelier selftest --client gui --golden` (`src/golden.rs`) captures
scripted GUI states at 170 × 46 cells that depend neither on the machine's
data nor on the clock (the demo canvas at 100 and 50 %, a context menu, the
new-session dialog, two tiled terminals printing fixed text, the reports
inbox and viewer, the Tasks board, a note in Source, the parity sandbox's
canvas with every status, the canvas visuals) and compares each with
`tests/golden/gui/<name>.png`:

- Animations hold one frame (the `freeze` hook), notices are waited out, and
  a capture counts once the screen text is still and two renders match.
  Regions that change legitimately are masked (hatched in the stored PNGs).
- `TOLERANCE`: a pixel differs when a channel moves by more than 24; at most
  0.5 % of the compared pixels may differ; no 8 × 8 block's mean luminance
  may move by more than 16.
- A failure writes `<name>.actual.png` and `<name>.diff.png` (red pixels,
  yellow blocks, blue masks) next to the golden (ignored by Git);
  `CANVAS_GOLDEN_DIR` puts the goldens elsewhere.
- `--update` rewrites every golden. Open each changed PNG (and its old
  version) and look at it before committing; re-bless only changes you
  intended, and say which in the commit message.
- The goldens belong to the reference machine (RTX 3070, 125 % DPI, its
  fonts): another GPU, DPI or font version changes sizes or anti-aliasing.

### Hardening

`atelier selftest --client gui --hardening` (`src/hardening.rs:run`)
runs nine checks with a table: the toolbar's counts at 170 columns,
overlapping boxes, a simulated and a real device loss, a DPI change, window
placement across restarts, the UI Automation tree as a UIA client sees it,
crash logs from the UI and a helper thread, the software-renderer fallback
and damage frames while terminals stream. It moves windows and crashes its
own GUIs on purpose, in its own sandbox.

### The terminal corpus

Recorded and synthetic output streams replayed into every engine of the
build and compared cell by cell (corpus/README.md):
`atelier corpus list | check [--engine NAME] [NAME…] | bless … |
diff [NAME…] | show NAME | synth`, and `record` to add an entry.
`cargo test --workspace` runs the same check
(`crates/canvas-corpus/tests/golden.rs`). An engine update is one reviewed
change together with the re-blessed expectations.

### Driving a GUI by hand

`atelier-gui.exe --test-hooks` serves a local, owner-only pipe
`\\.\pipe\atelier-gui-test-<pid>` that takes one JSON command per line
and answers one JSON line each (`canvas-gui/src/hooks.rs`; every command is
in docs/GUI.md §3). Useful to see what a failing row saw, in a sandbox:

```powershell
$sb = Join-Path $env:TEMP 'cc-hooks'
New-Item -ItemType Directory -Force "$sb\home", "$sb\sessions" | Out-Null
$gui = Start-Process .\target\release\atelier-gui.exe -PassThru -RedirectStandardOutput "$sb\pipe.txt" `
  -ArgumentList '--test-hooks', '--size', '170x46', '--demo', '--instance', 'hooks', '--home', "$sb\home", '--sessions-dir', "$sb\sessions"
$pipe = New-Object System.IO.Pipes.NamedPipeClientStream('.', "atelier-gui-test-$($gui.Id)", [System.IO.Pipes.PipeDirection]::InOut)
$pipe.Connect(30000)                                     # the pipe opens once the canvas is open
$writer = New-Object System.IO.StreamWriter($pipe); $writer.AutoFlush = $true
$reader = New-Object System.IO.StreamReader($pipe)
function Hook($command) { $writer.WriteLine(($command | ConvertTo-Json -Compress -Depth 5)); $reader.ReadLine() | ConvertFrom-Json }
(Hook @{ cmd = 'screen' }).lines                         # the screen as text
Hook @{ cmd = 'key'; char = 'f'; mods = @() } | Out-Null # show all nodes
(Hook @{ cmd = 'tree' }).tree.canvas.boxes | Format-Table name, status, rect
Hook @{ cmd = 'png'; path = "$sb\shot.png" } | Out-Null  # an offscreen capture
Hook @{ cmd = 'quit' } | Out-Null                        # saves and exits (detach)
.\target\release\atelier.exe service stop --instance hooks --home "$sb\home"
```

(Windows PowerShell 5.1 does not quote `-ArgumentList` items: keep the
sandbox path free of spaces.) `CANVAS_GUI_TRACE=1` prints start-up
milestones and GPU events on stderr.
The offscreen `png` never contains the WebView2 view (below).

**A test-hook window on a real canvas types into real sessions.** A second
`--test-hooks` window can join the owner's canvas (same instance, no
`--demo`) to make an edit, but it opens with the window's saved focus: often
a terminal, and then every `key`, `keys`, `text` and `paste` command goes to
that session's program (an agent's unsent draft). Before sending any key,
read `tree` and check that `focus` is not `Terminal`; leave a terminal with
`Ctrl+]` (it never reaches the program). Prefer clicks on `hits` and chips
by their action over typed keys, prefer a sandbox whenever the edit does not
have to be on the real canvas, and quit the window with `{"cmd":"quit"}`
(it detaches; the sessions keep running).

### Benches

`atelier bench [--terminals N] [--seconds S] [--json FILE] [--remote
loopback]` (`src/bench.rs`: an isolated service, N ConPTY terminals
streaming, keystroke latency through an echo terminal, UI render cost) and
`bench --burn N`; `bench --client gui` with `--stress`, `--perf-canvas
FILE|synthetic`, `--effects`, `--loops`, `--pulse`, `--burn N`
(`src/gui_bench.rs`, over the test hooks, opens a window). Results go to
README *Performance* and docs/GUI.md §4 when they change.

## 4. Continuous integration

`.github/workflows/ci.yml` runs on every push and pull request; a newer push
to the same branch cancels the older run.

| Job | Runner | Steps |
|---|---|---|
| `ghostty` | `windows-latest` | restores `target/ghostty` from the cache keyed by `crates/ghostty-vt-sys/vendor/CHECKSUMS.sha256`; on a miss installs Zig 0.17.0 (checksum checked) and runs `cargo xtask build-ghostty`; uploads the library |
| `test` | `windows-latest` | `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace`; `cargo build --no-default-features`; `cargo build --release`; `corpus check`; `selftest`; `selftest --remote loopback`; `selftest --remote faults`; `selftest --remote paired`; `selftest --client gui`; `selftest --client gui --parity`; `selftest --client gui --golden` with `continue-on-error` (it reports, it does not block); uploads `*.actual.png` and `*.diff.png` as `golden-diffs` |
| `ios` | `macos-15` | clippy and tests of the portable crates (`canvas-common`, `canvas-model`, `canvas-termview`, `canvas-push`, `canvas-mobile`, `canvas-mock`) and of `canvas-host --no-default-features`; `ios/build-core.sh`; XcodeGen; the app's unit and UI tests on an iPhone 16 simulator (booted first, one at a time, no hardware keyboard) against two `canvas-mock` services (waited for); a failure uploads the result bundle and the mocks' logs (`ios-tests`) |

Hosted runners have no GPU: the GUI runs on WARP at 100 % DPI, so its golden
screenshots differ from the reference machine's (RTX 3070, 125 %) without a
regression; the goldens only gate on the reference machine. CI does not run
`selftest --client gui --remote loopback`, `--hardening`, `--remote soak`,
`selftest agent` or the benches: run those locally (AGENTS.md lists the
order). To reproduce a Windows job, run its commands in the same order
after `cargo xtask build-ghostty`. The macOS job needs a Mac; on Windows,
`cargo clippy -p canvas-common -p canvas-model -p canvas-termview -p
canvas-push -p canvas-mobile -p canvas-mock --all-targets -- -D warnings`
catches warnings but not a Windows-only API in a portable crate.

## 5. Pitfalls

Each with the reason it matters.

- **`NodeKind` has no fallback variant.** `doc.rs:NodeKind` is `Session`,
  `Agent`, `Automation` with no `#[serde(other)]`: a canvas holding a new
  kind fails to parse, as a whole, in every older service and client. New
  kinds of boxes are an existing kind plus a field (canvas-wide agents are
  sessions with `source: "canvas-agent"`).
- **Postcard messages cannot gain fields.** `ClientMsg`, `ServiceMsg` and the
  structs inside them (such as `AgentLaunch` and `ClientInfo`) are encoded by
  position; an added field shifts everything an older peer reads.
  Carry extras in the JSON parts (`Routing`, `Bus`, `Rpc`, `DocEdit`) or, for
  a launch, in `AgentLaunch::env` (`canvas-model/src/launch.rs`:
  `GROUP_SKILLS_ENV`, taken out by the service before the agent starts).
- **Wire enums are append-only.** Postcard encodes the variant index, so a
  variant inserted in the middle renumbers the rest
  (`protocol.rs:protocol8_variants_round_trip_at_appended_indices` pins the
  indices). Even appended, a peer of an older build cannot decode a new
  variant: an older service drops a connection that sends an unknown
  `ClientMsg` (`canvas-host/src/server.rs`; a current service skips it and
  logs it once, but older services remain in use), so a new `ServiceMsg` goes only
  to clients that announce a capability bit (`CAP_*`,
  `ServiceMsg::shaped_for`). `Request` and `Response` travel as JSON by
  name; they are appended at the end too.
- **Older services refuse what they do not know in an edit.** An unknown
  `SetField` name or `Op` variant makes the whole edit fail
  (`service/mod.rs:doc_edit`): the service logs "refused a canvas edit",
  sends the window the whole document again and an error notice. Features
  that write a new shared field need a current service; say so.
- **The service never originates document patches.** Patches are clients'
  edits, applied in arrival order and broadcast with their origin
  (`DocPatch::origin`); each window's `Replica` relies on that to keep its own
  edits in flight. The service only gives a new canvas its `canvas_id` when
  it opens it. Service state goes in a store or the bus snapshot; a document
  change is made by a client (boxes several windows may create get
  `sync::stable_id` ids).
- **Routing covers one canvas at a time.** The service keeps the routing
  table of the canvas whose window sent it last (`ClientMsg::Routing`, from
  `canvas-core/src/passive.rs:sync_routing`; demo windows send none).
  Passive sessions, canvas-wide agents and the PR, review and todo hubs
  follow it. A test that opens another canvas on your instance re-routes
  your agents: use an instance of its own.
- **Glyphs must be one cell wide in the Windows console.** `☰` is two cells
  wide since Unicode 16 while the console draws it in one, which shifted the
  TUI's rows; `≡` is used instead. Check a new glyph with
  `paint::text_width` and in Windows Terminal; no emojis in code or UI text.
- **No rustfmt.** The code is not rustfmt-formatted (long lines are the
  style, and there is no `rustfmt.toml`); `cargo fmt` would reformat most of
  it. Match the surrounding code; clippy with `-D warnings` is the gate.
- **The bus inbox is not a trust boundary.** Any process running as the user
  can drop a file in `<home>/bus/inbox`. An event's `source` is the session it
  names, and an empty `source` is the user (`bus.rs:Event::source`). So
  requests are applied as the session they name (the rules keep agents in
  their lane; they are no sandbox), and what only the user may do comes from a
  window as `ClientMsg::Bus` and is ignored in the inbox (`canvas.stop_run`,
  review approvals: `passive.rs:bus_loop`), and what only the service may
  assert uses its own channel (`pr.merged` finishing todos).
- **The WebView2 view is a separate child window.** It is drawn over the GPU
  content, so the pane is hidden while a dialog or menu overlaps it
  (`webpane::pane_clear`), and offscreen `png` captures never contain it: check
  HTML with the `webview` hook (`eval`, `capture`, `window_capture`); the
  Notebook's golden shows a note in Source.
- **`NavigateToString` takes at most 2 MB.** A page over
  `canvas_core::reports::HTML_VIEW_MAX` (2,000,000 bytes) is shown as the
  core's formatted text with a note; HTML notes are served from the notes
  origin instead of passed as a string.
- **Viewing never starts a session.** A click, Tab, the arrows, a badge, a
  link, the recent-session keys, restoring tiles and reconnecting only view;
  only a double-click, Enter, an explicit Start or Resume or a menu item
  starts or resumes (WI-007). Tests check it by counting `launched ` in the
  service log.
- **The owner's machine is live.** Their canvas service, agent sessions and
  the TODO bot run there. Never stop a process you did not start (no
  `Stop-Process -Name atelier*`; stop a sandbox's service with
  `service stop --instance … --home …`), never write to `~/.atelier`
  or the owner's repositories, build at below-normal priority with a limited
  job count, and run one GUI suite at a time (they take the desktop and save
  and restore the clipboard).
- **Agent tool runners abort very large single writes.** Create big files in
  parts (write a first part, then append).
- **OpenCode loads every file named `AGENTS.md` it reads as instructions.**
  That is why the agent providers doc is `docs/AGENT-PROVIDERS.md`: name no
  other file `AGENTS.md`.
- **A new Git worktree has no Ghostty library.** `ghostty-vt-sys/build.rs`
  looks in `<workspace>/target/ghostty/<commit>/lib` (or
  `GHOSTTY_VT_LIB_DIR`). Copy `target\ghostty` from the main checkout rather
  than rebuilding; seeding `target\debug` from another checkout saves most of
  the cold build.
- **Skills reach agents only after the service restarts** on the new build
  (it writes `<home>/plugin` when it starts).
- **`--demo` alone uses your default instance.** Add `--instance` and
  `--home` for anything that is a test.
- **The TODO bot pushes to `main` every two hours.** It lives outside the
  repository (`C:\repos\copilot-canvas\todo-bot`, its worktree
  `C:\repos\copilot-canvas\wt-bot`): it implements an item of the owner's
  TODO list on a `bot/WI-nnn-…` branch, verifies it and pushes to `main`.
  `git fetch` before merging or rebasing, expect `main` to have moved, and
  leave its worktree alone.
- **Tests inherit the environment of the agent that runs them.** An agent
  session the canvas started (OpenCode, Claude Code, Copilot CLI) passes on
  its environment (`OPENCODE_CONFIG_CONTENT`, `ATELIER_SESSION`, …)
  and its Ctrl+C handling: processes started under OpenCode ignore Ctrl+C.
  Tests that depend on either fail there and nowhere else (in `canvas-host`
  `agents_the_canvas_stops_are_stopped_whatever_their_exit_code` sees `ping`
  ignore the stop's Ctrl+C). Keep tests hermetic (`opencode.rs` reads no
  ambient variable under `cfg(test)`), and when a failure only reproduces in
  an agent session, run the suite from a clean process, e.g. one created by
  WMI: `Invoke-CimMethod Win32_Process -MethodName Create -Arguments @{ CommandLine = 'cmd /d /c "cd /d <repo>&& cargo test --workspace > <log> 2>&1"' }`.

## 6. Releases and local installs

The README's *Quick start* describes a preview ZIP on the GitHub Releases
page holding `atelier.exe` and `atelier-gui.exe`, which must
stay in one folder. CI's Windows `test` job builds it right after clippy and
uploads it as the `windows-x64` artifact
(`atelier-<version>-<commit>-windows-x64.zip`: both executables and
`BUILD-INFO.json` with the build id, commit and SHA-256s); a pre-release is
that ZIP attached with `gh release create … --prerelease`. A local release build is
`cargo build --release` (both executables in `target\release`; the C runtime
is linked statically, `.cargo/config.toml`, because the service runs from
its copy in `<home>/bin`). `cargo build --release --no-default-features`
builds without Zig (alacritty engine). `cargo install --path .` puts only the
CLI/TUI on `PATH`; the GUI must sit beside the CLI it uses. Installing a new
build is replacing the files: the next client starts the new build's
service from its own copy, restarts an idle older one, and asks before
restarting one that runs sessions (README *Limitations*, Upgrading).

<!-- rename:keep-begin -->
## 7. Renaming to Atelier (formerly Copilot Canvas)

The product was renamed from Copilot Canvas to Atelier (the binaries
`atelier` and `atelier-gui`, the root package `atelier`, `ATELIER_*`
variables, `~/.atelier`). The workspace crates keep their `canvas-*` names
and "the canvas" stays the name of the session canvas. What belongs to
GitHub Copilot the agent keeps its names (`AgentKind::Copilot`,
`copilot_cfg`, `~/.copilot`, `COPILOT_*` variables, "GitHub Copilot CLI",
`github-copilot/...` model ids), and so do the iPhone app's bundle id
`com.thomashorga.copilotcanvas` and the repository URL (until the owner
renames the repository).

**The script.** `tools/rename-to-atelier.py` is the mechanical part: it
renames tracked files and folders whose path names the old product (`git
mv`), then rewrites text (`Copilot Canvas` → `Atelier`, `copilot-canvas` →
`atelier`, `COPILOT_CANVAS_` → `ATELIER_`, `com.copilot-canvas.` →
`com.thomashorga.atelier.`, …; in Rust, `std::env::var("ATELIER_…")`
becomes `canvas_common::env::var`, which falls back to the old name). It is
idempotent: what must keep an old name is in its `KEEP` list (the repository
URL, cryptographic context strings old and new builds share, the owner's Dev
Tunnel ids), in `crates/canvas-common/src/legacy.rs` (never rewritten), or on
a line holding `rename:keep` (or between `rename:keep-begin` and
`rename:keep-end`, in Markdown inside `<!-- -->`). View snapshots are not
rewritten: record them again. A branch made before the rename:

```sh
git merge origin/main                 # or rebase; take main's side where the rename conflicts
python3 tools/rename-to-atelier.py    # renames what the branch added
python3 tools/rename-to-atelier.py --check   # 0 change(s) needed
UPDATE_SNAPSHOTS=1 cargo test -p canvas-core --test view_snapshots   # then read the diffs
```

Easier still: run the script on the branch first, commit, then merge main
(the two sides then mostly agree). New code reads variables through
`canvas_common::env::var` / `var_os` and names old things only through
`canvas_common::legacy`.

**Compatibility, and where it lives.**

| Mechanism | Code | Tests |
|---|---|---|
| Variables: `ATELIER_X`, else `COPILOT_CANVAS_X`; sessions and passive runs get both names; the skill folders' launch entry keeps its wire name | `canvas-common/src/env.rs`, `service/mod.rs:Agents::integration`, `canvas-model/src/launch.rs` | `env.rs` tests, `launch.rs`, `service/passive.rs` |
| The data home moves on the first start when no service uses it (paths inside rewritten, preferences renamed, an interrupted move finished); `atelier migrate`; `doctor` | `canvas-agents/src/home_move.rs`, `paths.rs:home_in`, `src/main.rs:migrate_home` | `home_move.rs` tests (temporary homes only) |
| A service of the old build on its old endpoint (Windows pipe, Unix socket folder) is found and replaced through the update path; a new service refuses to start beside one | `paths.rs:Instance::legacy_pipe_name`, `conn.rs:connect_endpoint`, `canvas-host/src/server.rs:run` | `paths.rs` tests, `home_move.rs` (an old endpoint is found) |
| Scheduled entries with old names are the same automations, replaced by `apply` / `service install` | `scheduler/launchd.rs`, `systemd.rs`, `cron.rs`, `factory.rs:rename_legacy_manifests`, `factory_cli.rs:apply` | a test per backend and `factory.rs` |
| The `copilot-canvas` alias of the CLI for sessions (deprecated) | `cli.rs:install_alias`, `src/main.rs:main` | `cli.rs` |
| Keychain / Secret Service master keys under their old names | `canvas-sys/src/secret.rs:legacy_items` | `secret.rs` |
| `atelier://` and `ccanvas://` pairing links | `canvas-common/src/remote.rs:PairLink`, `ios/Atelier/Features/Pair/PairView.swift:isPairLink` | `remote.rs`, `CoreBridgeTests` |

**On a development machine** the new binary run without `--home` (or
`ATELIER_HOME`) moves `~/.copilot-canvas` to `~/.atelier` when no service
uses it: test with `--home` or a temporary `HOME`.
<!-- rename:keep-end -->

## 8. Which docs a change updates

| Change | Docs |
|---|---|
| Anything a user can see | README *Features* (and *Controls*, *Commands* when keys or commands change), docs/PARITY.md (a row and *Last run*) |
| Protocol, backend requests, stores, compatibility | docs/BACKEND.md |
| The GPU client, test hooks, measurements | docs/GUI.md |
| Agent launch, status, skills, models | docs/AGENT-PROVIDERS.md |
| Notebook, Tasks, spaces | docs/NOTEBOOK-TASKS.md |
| Remote mode, pairing | docs/REMOTE.md |
| The terminal engine | docs/GHOSTTY.md, corpus/README.md |
| The iPhone app | ios/README.md, docs/IOS-STATUS.md |
| How to work here | AGENTS.md (short), this guide, `.claude/skills/` |
| Status and backlog | PLAN.md §0 |

