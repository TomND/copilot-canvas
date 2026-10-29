# Copilot Canvas

A fast, native, spatial session manager for agent CLIs on Windows:
**GitHub Copilot CLI, OpenCode and Claude Code**. Sessions are boxes on an
infinite canvas; subagents hang off their parent; groups are boxes around
sessions; the sidebar lists everything; clicking a box shows its **real**
agent terminal on the right when it runs (single or tiled), or its details with
an explicit Start / Resume button; double-click or Enter starts it. Two front ends share one
persistent terminal service: a terminal UI (TUI) and a GPU client (GUI); both
work locally or from another PC over a Dev Tunnel (remote mode).

```
┌ SESSIONS ──────────┐┌ canvas ─────────────────────────┐┌ terminals ───────────────┐
│ ▾ ▣ Payments    3  ││ ┌╌ ▣ Payments · 3 ╌╌╌╌╌╌╌╌╌╌┐   ││ ● Fix flaky test · working│
│   ⠹ Fix flaky test ││ ╎ ╭─ Fix flaky test ─╮      ╎   ││ copilot> …               │
│     ◇ Explore      ││ ╎ │ ⠹ working        │┄┄┄╮  ╎   ││                          │
│   ↳ ✓ Write tests  ││ ╎ ╰──────────────────╯   ┊  ╎   ││                          │
│ ── Ungrouped (1)   ││ ╎   ┌ ◇ Explore ─┐  ╭─ ↳ Write tests╮                        │
│ ↗ ⠸ Review PR      ││ └╌╌╌└────────────┘╌╌╰───────────────╯◢                        │
└────────────────────┘└─────────────────────────────────┘└──────────────────────────┘
```

Design and status (layers, remote mode, libghostty-vt, GPU client): see [PLAN.md](PLAN.md).

## Quick start

```powershell
git clone <this repository> copilot-canvas
cd copilot-canvas
cargo xtask build-ghostty                      # once: builds libghostty-vt (needs Zig 0.17.0; docs/GHOSTTY.md)
cargo build --release                          # copilot-canvas.exe (service, CLI, TUI) + copilot-canvas-gui.exe
.\target\release\copilot-canvas.exe --demo     # safe demo: PowerShell panes, no agent
.\target\release\copilot-canvas.exe            # your real agent sessions (TUI)
.\target\release\copilot-canvas.exe --gui      # the same canvas in the GPU client
```

Optional: `cargo install --path .` puts `copilot-canvas` on your PATH. Use a
reasonably large Windows Terminal window for the TUI (≥ 140×40 is comfortable).
Without Zig, `cargo build --release --no-default-features` builds with the
alacritty terminal engine instead of Ghostty.

- **Agents.** New sessions use the first installed of Copilot, OpenCode and
  Claude Code; pick another per session in the new-session dialog (*Agent*), per
  canvas in *Settings › Agent for new sessions*, or for the machine with
  `copilot-canvas config agent opencode`. Each agent's sessions, status,
  subagents and configuration views come from its own history and CLI. See
  [docs/AGENTS.md](docs/AGENTS.md) for how each agent is launched, resumed and
  forked, how its status is read, and what is verified end to end.
- **Clients.** `copilot-canvas` opens the client set by
  `copilot-canvas config client tui|gui|auto` (default `tui`; `auto` picks the
  GUI except over remote desktop or SSH); `--tui` and `--gui` override it. In
  either client, **O** opens the same canvas in the other one (same service,
  same document). See [docs/GUI.md](docs/GUI.md).
- **Remote mode.** Agents run on one PC (say a Cloud PC), the canvas on another,
  over a private Dev Tunnel with pairing: `copilot-canvas remote tunnel-setup`
  and `remote pair` there, `remote add NAME --tunnel ID --code CODE` and
  `copilot-canvas --remote NAME` at home. See [docs/REMOTE.md](docs/REMOTE.md).
- **Terminal engine.** Terminals run on libghostty-vt (graphemes, Kitty
  keyboard, underline styles, images); `copilot-canvas config engine alacritty`
  switches back. See [docs/GHOSTTY.md](docs/GHOSTTY.md).

On first run the canvas imports your 8 most recent sessions. If the Node
prototype's `~/.copilot-canvas/layout.json` exists, its layout is migrated.

## Features

| | |
|---|---|
| **Canvas** | Drag boxes (subagents follow their session), drag empty space to pan, wheel/Shift+wheel pan, Alt+wheel / Ctrl+wheel / toolbar − + to zoom (100 %, 50 %, 25 %), double-click empty space to create a session there, right-click for context menus |
| **Status** | Accurate within a fraction of a second for canvas sessions: Copilot's own progress report (set when a turn starts, cleared when it ends) drives working and done; history events add needs-you, failures and sessions running elsewhere; a session whose subagents are still running stays working. States: working (spinner), needs you (permission / question), **done · review** (finished a turn you haven't looked at yet; stays until you open, click or type in that session), idle, stopped, failed, and **↗ open elsewhere** for sessions running in another terminal or VS Code. Sessions that **need you** get a gold heavy ━ border and sessions that are **done · review** a blue double ═ border with a blue-tinted box (boxes, terminal panes and sidebar names). Pressing Esc in Copilot (abort) doesn't count as done. The session whose terminal is active on the right has a blue halo around its box (heavy while you type in it) |
| **Colour-blind safe** | The palette avoids red/green: OK is sky blue, failure is vermillion, attention is gold, finished is blue, selection is white, groups use the Okabe-Ito colours; every state also has its own icon and border shape |
| **Subagents** | Discovered from session history and drawn as linked child boxes |
| **Inspect transcript** | Any session or subagent box (menu, details panel): a read-only transcript of its conversation (prompts, answers, tool calls with their result or exit code), newest at the bottom, refreshing every 1.5 s while it runs; nothing starts, nothing is written, it works remotely too. A finished OpenCode child session whose parent is stopped resumes in a terminal of its own as a new linked subsession (**Resume as subsession** or a double-click); Copilot and Claude Code subagents are part of their parent's conversation (see [docs/AGENTS.md](docs/AGENTS.md#inspecting-and-resuming-child-conversations)) |
| **Subsessions** | `S` or menu → a new Copilot session linked to its parent (dotted connector, nested in the sidebar). By default it **forks the parent's conversation** (as Copilot's `/fork` and VS Code do: history and checkpoints are copied, the running parent is untouched); choose *Start fresh* for an empty one. Sessions an agent starts itself from inside a canvas session (`copilot -p …`, `copilot --resume …`) are detected from the process tree and added as linked subsessions automatically |
| **Stacks and hidden subsessions** | Right-click a session › *Stack subsessions* (or `K`) gathers all its subsessions (recursively, with their subagents) into a deck beneath it: offset cards, the newest on top, a **+N** badge, and the stack's most important status (needs you, failed, done · review, working) on the top card, so stacking never hides activity; effects on a stacked session show on the deck. A click on the deck shows its top card (it never starts anything); dragging it moves the parent with its stack; a double-click, `K` or the deck's menu unstacks: the boxes go back exactly where they were (their positions stay in the canvas, so this survives restarts and every window sees it). `H`, the canvas menu or the toolbar's **↳ N hidden** hides every subsession box (each parent shows **⋯ N hidden**) and shows them again in place. The GUI animates both (about 300 ms, eased, reversible mid-way, then idle); the TUI switches at once |
| **Looping sessions** | A session that repeats on a schedule gets a dotted loopback arc on its right side with one lit dot running around it (bluish green), `↻ every 15m` in its status line and `↻` in the sidebar. Detected from Copilot's schedule events (`/every`, the `manage_schedule` tool: created → looping, cancelled or session ended → not), Copilot's saved session schedules, and enabled repeating automations. Zoomed out, a pulsing `↻` |
| **MCP servers per session** | The new-session dialog has an *MCP servers* row: a checklist of your servers with presets (*All*, *None*, and your own: `Ctrl+S` saves the current choice, `←/→` switches, `Del` deletes). The last choice is the default next time; each session remembers its own choice for every resume (`--disable-mcp-server`). Change it later from the session's menu → *MCP servers…*. Every server is several processes in every session, so trimming them is the biggest saving in memory and CPU |
| **Groups** | Shift-drag or right-drag to select, then `G` (or menu) draws a group box around them; a click on a group's empty background or its border (a few pixels around the outline in the GUI) selects it (the innermost of nested groups; a drag there still pans); drag the title to move everything inside, drag `◢` to resize; nested groups work; membership is geometric, like Obsidian canvas |
| **Sidebar** | Groups → their folder row (◧) → sessions → subsessions → active subagents → the group's passive sessions (⟲, a click opens their panel); stacked and hidden subsessions are listed dimmed, unused sessions with their age; `/` filters; click any row to fly the canvas to it, highlight it and view it (its live terminal, or its details); double-click or Enter starts it; group rows tile the group's running terminals (never starting stopped ones) |
| **View versus start** | A click (canvas or sidebar), Tab and the arrows only select and show: a running session's terminal comes into view with the keyboard, anything else shows a details panel (agent, status, folder and branch, last used, what starting it would do) with one primary button: **Start** (a new conversation or a box's command), **Resume** (its conversation) or **Open terminal**, plus Stop, MCP servers… and Remove. Only a double-click, Enter, that button or a menu item starts a session. Restoring tiles, reconnecting and clicks on exited terminals start nothing; sessions open elsewhere (↗) never get a Start / Resume |
| **Terminals** | Real Copilot CLI in ConPTY; single view or up to four tiles (`T`); terminals keep running when you close the canvas and reappear when you reopen it |
| **Scrolling** | The wheel goes to the app like in Windows Terminal: mouse-wheel events when Copilot's `mouse` setting is on (its default; 3 lines per notch), PageUp/PageDown on its full-screen view when it is off, scrollback otherwise; Shift+wheel or Shift+PgUp always scroll the scrollback |
| **Your messages** | In Copilot terminals your own prompts get a navy background and a blue ▎ bar so they stand out from Copilot's output (recognised from how Copilot draws them: " ❯ text" and its indented continuation lines) |
| **Copy & paste** | Drag inside a terminal to select; releasing copies it. Copies keep line breaks (CR LF), blank lines, indentation and spaces inside lines; lines a shell soft-wrapped rejoin (the terminal engine's own wrap state, so a line that just fills the width or a border in the last column never joins the next row). On full-screen views (Copilot, OpenCode, vim) every row is a line, and Copilot's scrollbar column is left out. Ctrl+C / Ctrl+Shift+C copy while text is selected (otherwise Ctrl+C reaches Copilot); Ctrl+V / Ctrl+Shift+V paste; the pane's right-click menu has Copy / Paste. **A paste never sends a message**: multi-line text (Ctrl+V, the pane menu, Windows Terminal's paste, IME text) reaches the agent as one bracketed paste whose line breaks stay text, a trailing line break is dropped, and a program without bracketed paste gets Shift+Enter or Alt+Enter between lines, never Enter ([docs/AGENTS.md](docs/AGENTS.md#pasting)) |
| **Images** | Copy a screenshot and press **Alt+V** (or Ctrl+V, or the pane menu's *Paste*): Copilot attaches it (`[📷 …png]`). With no text on the clipboard the canvas sends the keystroke and Copilot reads the image itself. Windows Terminal keeps Ctrl+V for its own text paste, so with only an image on the clipboard Ctrl+V never reaches any app there; use Alt+V |
| **Copilot's terminal features** | Clicks reach Copilot when its `mouse` setting is on (its tab bar, "click here" actions); drags still select text in the canvas. Copilot's title (its current intent) shows in the pane header. Its progress report (OSC 9;4) drives the spinner instantly and the canvas mirrors overall state to the Windows Terminal tab and taskbar (busy while any session works, yellow while one needs you). Bells and OSC 777 notifications (Copilot's `beep` / `terminalNotifications` settings) ring Windows Terminal |
| **Links** | URLs, OSC 8 hyperlinks and absolute Windows paths are underlined. In terminals Ctrl+click opens them, elsewhere a click does. Web links open in your browser, folders in Explorer, files in VS Code at the line (or revealed in Explorer); nothing is ever executed |
| **New session** | Name, folder (type, **Browse…** folder picker, or **Recent ▾**), Git detection with current branch, **New Git worktree** (base branch picker, new branch, path), the **Model** it starts with (see below), optional first prompt |
| **Default model** | The new-session dialog shows the model a session will start with and why: the group's (*from group X*), the one you chose before (*remembered default*), or the initial default, **Claude Opus 5.5** in each agent's own id (`claude-opus-5.5` for Copilot, `github-copilot/claude-opus-5.5` for OpenCode, `claude-opus-5-5` for Claude Code). Its picker lists the agent's models (`opencode models`, the choices in `copilot --help`, Claude Code's aliases) and takes any id you type. A model you pick there starts that session and becomes that agent's default for every later new session (kept in the canvas, so it survives restarts and reaches your other windows) until you pick another, there or in *Settings › Copilot / OpenCode / Claude Code model* (which also resets it). A group's own model (*Session defaults for group…*) wins in that group; picking another there applies to that session only and changes neither the group nor the canvas default. Every session records the model it started with (details panel) and keeps it: a new default never changes existing sessions or their resumes ([docs/AGENTS.md](docs/AGENTS.md#models)) |
| **Import / sync** | `I` Copilot sessions (filter, multi-select, group by folder or client); `V` or toolbar **⟳ VS Code** syncs VS Code agent sessions (see below) |
| **Activity bar** | VS Code-style icons on the far left (`Alt+1…8`): ▤ Sessions · ◷ Automations · ⧉ Plugins · ⌁ MCP servers · ✦ Skills · ◈ Agents & instructions · ⇄ VS Code · ≡ Settings |
| **Dusty sessions** | Sessions you have not used for a while (started, typed into, or agent activity; looking at one does not count) gather dust: the TUI dims them, the GUI drains their colour and adds specks and a cobweb that grow with age (from *Settings › Dust unused sessions after*: off / 1 / 3 (default) / 7 / 14 days; medium from 14 days, heavy from 60), and the status line and the sidebar show **unused 12d**. Anything important (working, needs you, done · review, failed, open elsewhere, looping, passive sessions, an effect) is never dusty. The canvas menu's *Select unused sessions* selects them for a cleanup; nothing is ever removed automatically |
| **Effects** | Passive sessions (and sessions) can put an effect on a box of their group until you click it: **fireworks** (an approved pull request), **attention** (a pulsing beacon: review requested, changes requested, comments), **sparkle** (a short success), **alert** (`copilot-canvas effect`, below). The service keeps them, so every window and the TUI show them, also after a restart. A click on the effect (or its box) clears it everywhere and only selects the box. GUI: fireworks bursts and rings at 12.5 frames/s, only while in view; *Settings › Effects*: on, reduced motion (still rings and badges) or off (badges only); the default follows Windows' *Show animations*. TUI: a badge (✦ ◉ ✧ ▲) on the box |
| **Group folders** | Each group has a folder (asked when you create the group; suggested `~/.copilot-canvas/groups/<name>`) with `ai-docs/` and `human-outputs/`. Open them from the group's title chips **◧ AI docs** / **◨ Outputs** (compact **◧** / **◨** when zoomed out or narrow, **◧ Set folder…** before a folder is chosen; the chips stay clickable at every zoom while the title shows), the sidebar's **◧** row under the group (a menu: AI docs, outputs, the group folder, `GOALS.md`, change folder), its menu, or its details panel. In remote mode (any `--remote`, `loopback` included) folders open in VS Code through the server's tunnel or in the canvas's **folder browser** (subfolders and files; Enter or a click opens a file in the read-only viewer, Backspace goes up, *Copy path*), never in Explorer on the service's machine. Sessions launched in the group get `--add-dir` for both and group instructions (via `COPILOT_CUSTOM_INSTRUCTIONS_DIRS`) telling Copilot to write notes to `ai-docs` and deliverables to `human-outputs` |
| **Copilot defaults** | Settings view: permissions (`allow-all` by default, or `ask`), each agent's model for new sessions, reasoning effort, start mode, custom agent, context tier, extra `--add-dir` folders for new sessions; any group can override them (group menu → *Launch defaults*). Also lists `copilot config` settings and edits them through `copilot config` |
| **Plugins · MCP · skills** | Lists come from `copilot plugin/mcp/skill/instruction list --json`; Space enables/disables, Del removes (confirmed; servers and skills that come from a plugin, the project or Copilot itself can only be disabled), **+** rows add a skill, an MCP server (`copilot mcp add` syntax) or install a plugin (runs in a canvas terminal so you can answer prompts). Secrets (env, headers) are never shown |
| **Automations** | Reads Copilot's automations (`~/.copilot/data.db`: scheduled workflows, their last 10 runs, and per-session schedules). They appear in the Automations view and as ◷ boxes in a special **Automations** group on the canvas. Click one for its prompt, schedule, model, mode, project, last/next run and run history; Space or the button enables/disables a scheduled workflow (per-session schedules are managed with `/every` in that session). Refreshes every 15 s |
| **Migrate from VS Code** | VS Code view → *Migrate skills & settings…* previews VS Code (Code, Insiders, all profiles) MCP servers, instruction files, prompt files (as skills), custom agents / chat modes and skill folders, marked new / already present / conflict / manual. Applies only the items you tick: backs up `~/.copilot` config first, never overwrites, writes a redacted `migration-log.txt` |

## Controls

| Action | Mouse | Keys |
|---|---|---|
| View a session (its live terminal, or its details) | click box or sidebar row | Tab cycles, arrows move |
| Start / resume a session and focus its terminal | double-click box or sidebar row; details panel **Start** / **Resume**; menu | Enter |
| Move | drag box (or group title) | arrows (Shift = faster) |
| Pan | drag empty space, middle-drag, wheel | Ctrl+arrows |
| Zoom | Alt+wheel, toolbar − + | `+` `-`, `F` fit all, `C`/Home center |
| Select several | Shift-drag or right-drag | Tab cycles sessions |
| Group selection | context menu | `G` |
| Select a group | click its empty background or its border (or its title, its sidebar row) | arrows move it, Del deletes the group box, `R` renames |
| New session / subsession | toolbar **+ New**, double-click, menu | `N` / `S` |
| Rename · remove · stop | context menu | `R` · Del · `X` |
| Remove several boxes (running sessions kept, history kept; asks first) | select them, right-click one → **Remove N from canvas** | Del; Ctrl+Z restores them |
| Import · sync VS Code | toolbar **Import**, **⟳ VS Code** | `I`, `V` |
| Switch side view | activity bar | `Alt+1` … `Alt+8` |
| In a list view | click row | ↑↓, Enter open/edit, Space enable/disable, Del remove, Esc back |
| Tile terminals | toolbar | `T` |
| Leave a terminal | click canvas/sidebar | **Ctrl+]** or F6 |
| Copy / paste in a terminal | drag to select; pane menu | Ctrl+C (with selection) · Ctrl+V / Ctrl+Shift+V |
| Open a link | click (Ctrl+click in terminals) | — |
| Undo the last VS Code sync or removal | canvas menu, VS Code view | Ctrl+Z |
| Quit (sessions keep running) | toolbar **⏻ Quit** | `Q` / Ctrl+Q, even while filtering (asks: detach or stop all) |
| Stack / unstack subsessions | session menu; double-click the stack | `K` |
| Hide / show all subsessions | canvas menu; toolbar **↳ N hidden** | `H` |
| Clear an effect | click it (or its box; nothing starts) | — |
| Select unused (dusty) sessions | canvas menu › *Select unused sessions* | then Del removes them from the canvas |
| Help | toolbar `?` | `?` |

While a terminal has focus, every key goes to Copilot (including Ctrl+G and
Ctrl+Q). Only **Ctrl+]** and **F6** are reserved, plus Ctrl+C while text is
selected and Ctrl+V (paste), as in Windows Terminal.

## VS Code sync

`V` (or the toolbar / VS Code view) adds VS Code agent sessions that are not on
the canvas yet. Boxes are named with VS Code's session and chat titles; existing boxes are renamed to them too unless you renamed the box on the canvas (VS Code renames carry over on the next sync). Each Agents-window session's **sub-sessions** (extra chats) come along as linked subsession boxes next to it. A session is synced when it was **created in VS Code**
(Copilot records `client_name: vscode-agent-host`) or when you filed it into an
**Agents-window group**, and it was active within the window (default 48 h;
change it in the VS Code view). Sessions VS Code merely lists because they were
started in Copilot CLI (scripts, pipelines, terminals) are skipped unless you
tick *Include CLI-started sessions*; archived ones unless you tick *Include
archived*. Placement: Agents-window group → canvas group of the same name;
otherwise the VS Code workspace; otherwise the session's folder
(`VS Code · <folder>`). A session filed in both a stable VS Code and an
Insiders group goes to the stable one.

Sync **only adds**: new boxes, sub-session links and names (for boxes you have
not renamed). It never moves existing boxes or removes groups, so your
arrangement is yours. New boxes are placed so a group never grows over its
neighbours (overflow goes to a "Name (2)" group). Before each sync the canvas is
saved to `~/.copilot-canvas/backups/` (newest 30 kept) and **Ctrl+Z** undoes the
last sync. *Sync when canvas opens* runs it on start. All VS Code storage is
read-only.

## Passive sessions and events

A group can hold **passive sessions**: ordinary Copilot sessions that watch the
group instead of being driven by you, e.g. a drift guard that checks every PR
push against the project's goals, or a rollout tracker that follows a merged PR
daily. Right-click a group › **New passive session…** and pick a template
(*Drift guard*, *Rollout tracker*, *Custom*). They are drawn dashed with ⟲ in
the group's passive area; clicking one opens its panel (status headline, next
run, triggers, recent group events, Run now / Pause / Edit / Open chat / Open
folder).

- **Files are the source of truth**: `<group>/.canvas/passive/<slug>/rules.md`
  (what to do; edit freely), `state.md` (its memory between runs), `log.md`,
  `group-context.md` (refreshed digest of the group's other sessions), plus
  `<group>/ai-docs/GOALS.md` and the group's event catalog
  `<group>/.canvas/events.yaml`.
- **Triggers**: subscribed events (`pr.pushed`, `pr.*`, `*`, optional text
  filter), *every N minutes*, *daily at HH:MM* (local time) and follow-ups it
  schedules itself. Events arriving within 8 s are coalesced into one run.
- **Runs** are headless `copilot --resume <id> -p …` turns started by the
  service (no process stays alive between runs, at most 2 at a time), so it is
  one continuous conversation. A passive session runs the canvas's default
  agent (`opencode run -s ses_… --auto`, `claude -p -r …` for the others) with
  the model new sessions of that agent in its group get (the group's, the
  remembered or the initial default), on every run; a conversation whose
  model is recorded on its box (you started its chat) keeps that model when a
  run resumes it. After *rollover* runs (default 20) it starts a
  fresh conversation that reads its files; old ones are kept as episodes. If
  you have its chat open and idle, the run is typed in there instead.
  Budget: *max runs per day* (default 24); failures back off 1 / 5 / 30 min
  and alert after 3.
- **Events**: sessions launched from the canvas get the `canvas-events` and
  `canvas-passive` skills (`--plugin-dir ~/.copilot-canvas/plugin`) and the
  `copilot-canvas` CLI on `PATH`. `git push`, PR creation/update/merge/comments
  (git, `gh`, `az repos`, ADO MCP tools) are also **detected from session
  history** automatically. You can send one by hand from a group's menu
  (*Send event…*). Events pulse on the canvas.
- **Effects**: `copilot-canvas effect fireworks|attention|sparkle|alert --on NAME
  [--note TEXT]` puts an effect on a box of the sender's group (`--on group`:
  the group) until someone clicks it; `effect clear --on NAME [--kind K]`
  takes it off. Only boxes of the sender's own group can be named, and the
  note is one short line of untrusted text. The group's event catalog can map
  event types to effects (`effects:` `pr.approved: fireworks`; new catalogs map
  `pr.approved`, `pr.changes_requested` and `pr.review_requested`), shown on
  the sending session's box. The *Rollout tracker* template celebrates
  approvals with fireworks and marks review requests and comments with
  attention; the *Drift guard* marks scope creep.
- **Within reach**: a group's title row shows **⟲ N** (its passive sessions;
  a click lists them, each opens its panel) or **⟲ + passive**; the sidebar
  lists them under their group; the panel opens rules, memory, log, the group
  context and `GOALS.md` (in remote mode in VS Code through the tunnel or the
  read-only viewer).
- **Messages**: passive sessions (and sessions) can message any session in the
  group with `copilot-canvas send`. A message is typed in only when the target
  is idle (never mid-turn); otherwise it waits with a ✉ badge (menu: *Deliver queued
  messages* / *Discard queued messages*). *Mute passive messages* turns this off
  per session.

```text
copilot-canvas events                         this session's group: events, passive sessions, sessions
copilot-canvas emit TYPE [--data JSON] [--to NAME]
copilot-canvas send --to NAME TEXT
copilot-canvas status ok|warn|alert|done HEADLINE      (passive sessions; alert rings the bell)
copilot-canvas followup --in 24h|--at 09:00 NOTE        (passive sessions)
copilot-canvas effect fireworks|attention|sparkle|alert --on NAME [--note TEXT]   (until clicked; effect clear --on NAME)
```

The bus lives in `~/.copilot-canvas/bus/` (`inbox/`, `routing.json`,
`state.json`, `log.jsonl`). The service keeps running while passive sessions
have schedules or pending work.

## Automations (scheduled tasks from manifests)

Copilot Canvas manages Windows Task Scheduler automations from **one YAML
manifest per automation**, kept in a folder (ideally a git repository; set it
in *Settings › Automations*, with `COPILOT_CANVAS_AUTOMATIONS`, or `--dir`).
The manifests are the source of truth; Task Scheduler is generated from them.

```yaml
name: 'ExcelTeams-Morning'
family: 'Excel Teams pipeline'       # canvas row and sidebar section
enabled: true
ai: true                             # runs cost credits (the doctor re-runs these sparingly)
command: 'C:\Program Files\PowerShell\7\pwsh.exe'
args: '-File "...\Run-Pipeline.ps1" -Kind morning'
cwd: 'C:\Obsidian\NewWorkNotes'
schedule: ['logon', 'daily 06:45']   # daily HH:MM · weekly mon,fri HH:MM · every 2h [from HH:MM] · logon
time_limit: '3h'
overlap: ignore                      # ignore · queue · parallel · stop
restart_on_failure: '3 every 1m'     # optional
success_codes: [0]                   # optional: exit codes that mean it worked
report: '"python.exe" "reporters\status.py" excel-brief --edition morning'
docs: ['...\Policy.md']             # owner rules the doctor must follow
logs: ['...\Runs\*_morning_*\stderr.log']   # workload logs (newest match)
outputs: [{title: 'Morning brief', path: '...\Latest.md'}]
```

A folder-level `automations.yaml` names the hidden launcher (`launcher`,
windowless `python`) and its `state` folder (`tasks/`, `logs/`, `status/`,
`backups/`).

- **`copilot-canvas automations import`** writes manifests for existing tasks;
  **`apply`** shows what would change in Task Scheduler and **`apply --yes`**
  makes it so (previous definitions are backed up). Manifest times are local.
  Task Scheduler stores fixed instants (a time without an offset counts as
  UTC), so times are written with the current offset and show as drift after a
  daylight-saving change until applied again; a nightly `apply --yes` task
  (the `Automations-Sync` example) keeps them on local time. `list`,
  `show NAME`, `run|enable|disable NAME` do the rest (enable/disable edits the
  manifest too).
- **Runs** go through the launcher, which logs `Starting task.` / `Exited with
  code N.` (and launch failures) per task and gives the run
  `AUTOMATION_NAME`, `AUTOMATION_RUN_ID` and `AUTOMATION_STATUS_FILE`.
- **Status reports**: a run (or its manifest's `report` command, run after it
  with `AUTOMATION_EXIT_CODE`) writes JSON to `AUTOMATION_STATUS_FILE`:
  `state` (ok/warn/alert/failed), `headline`, markdown `summary`, `links`,
  per-item `metrics`. `copilot-canvas automations report STATE HEADLINE
  [--summary …] [--link T=URL]… [--metric NAME=VALUE[=STATE]]…` writes one.
- **On the canvas** automations are double-bordered ◷ boxes in the Automations
  group, in rows per family; the sidebar's Automations view lists them by
  family. A failed run (or a `failed`/`alert` report) turns the box red and
  pulsing until the next good run, and the toolbar shows **✗ N automations
  failing** (click: the list). Clicking a box opens its **info and health
  page** (no terminal): state, run history strip, the latest report with
  metrics and links, next run, drift between manifest and task, command, and
  the tails of the launcher and workload logs; buttons run it now,
  enable/disable it, ask the doctor, and open outputs, manifest and logs.
- **The Doctor**: when the Automations group first holds manifest automations,
  a passive session *Doctor* (template *Automation doctor*) is added. The
  service watches every automation's launcher log and sends
  **`automation.failed`** (once per incident, then at most every 12 hours) and
  `automation.recovered`; *Ask doctor* sends `automation.check`. The doctor
  reads the manifest and its owner docs first, finds the root cause, fixes it
  locally within those rules (tests, lock files, maintenance logs, local git
  commits; never pushes), re-runs the automation to verify, and reports.
- **Unattended**: `copilot-canvas service start` (e.g. from a logon task, see
  the `CopilotCanvas-Service` manifest example) keeps the service, and so the
  doctor and passive schedules, running without the canvas open.

## Architecture

```
 clients (home PC or local)                    copilot-canvas service (persistent, per user; where agents run)
  copilot-canvas.exe      TUI (ratatui)   ◀──▶   L1 terminal host: ConPTY per session, libghostty-vt
  copilot-canvas-gui.exe  GUI (wgpu)             (or alacritty) per terminal, protocol-8 row-diff frames
  canvas-core: app state, ViewModel              L3 backend: agent providers (Copilot, OpenCode, Claude),
  canvas-termview: terminal grids                history index, canvas document owner, event bus,
  ~/.copilot-canvas/prefs (own view)             passive sessions, automations, backend RPCs
        local: owner-only named pipe, HMAC token · remote: TCP on loopback behind a Dev Tunnel,
        paired client secret (DPAPI), heartbeat, reconnect, input acks, zstd
```

Workspace crates: `canvas-common` (protocol, transports, settings),
`canvas-host` (L1), `canvas-agents` (L3 backend), `canvas-model` (document,
bus, RPC types), `canvas-core` (app state machine), `canvas-termview` (L2),
`canvas-tui`, `canvas-gui`, `canvas-corpus` (golden terminal corpus),
`ghostty-vt-sys` and `xtask`.

- **The service owns the canvas document.** It is the single writer of
  `canvas.json`; every open window sends its edits and applies the patches the
  service broadcasts, so two windows (say one at home over `--remote`, one on
  the Cloud PC) can edit one canvas at once. Each window keeps its own view
  (position, zoom, panels, tiles). Everything else a window needs from the
  machine (folders, Git, agent configuration, automations, VS Code) goes
  through the backend API. See [docs/BACKEND.md](docs/BACKEND.md).
- **The service owns all terminals.** Closing or crashing the UI never stops a
  session; reopening shows the same live terminal with its history. It runs
  from a per-build copy in `~/.copilot-canvas/bin`, so rebuilding is never
  blocked, and exits after 10 idle minutes with no clients and no terminals.
- **Hidden terminals cost almost nothing to the UI.** Every terminal's output is
  parsed in the service (state stays correct), but only visible tiles are
  diffed and streamed, row by row, at up to 250 frames/s.
- **The UI thread never waits on the console or on sessions.** It handles
  input first, spends at most 4 ms per pass on service updates (session status,
  terminal frames), and renders frames in memory; a separate output thread
  writes each frame to the console in one synchronized write. If the console is
  slow, frames simply arrive at its pace and always show the latest state. The
  window size comes from resize events (never a console query per frame). The
  canvas and the service run above normal priority, so Copilot processes that
  are starting or working cannot starve them (they still run at normal
  priority themselves).
- **The session you type in comes first.** The service gives the whole process
  tree of the session that last received your keystrokes (Copilot, its tools,
  MCP servers and anything it builds) above-normal CPU priority, sessions shown
  on screen normal priority, and every other session below-normal priority.
  Background sessions still use all spare CPU; they just yield to your typing.
  Re-applied every 2 s so new child processes follow. Measured with 40
  CPU-burning background sessions on a busy 16-core machine: keystroke→screen
  p50 257 ms → 5 ms, p99 1.26 s → 14 ms. Disable with
  `COPILOT_CANVAS_NO_PRIORITY=1`.
- **No artifacts.** Characters whose width terminals disagree on (emoji,
  symbols that gained an emoji form, Unicode-16 width changes) are followed by
  an explicit cursor move, so a disagreement can affect at most that one cell.
  Every 10 s, soon after a slow console write, and on **Ctrl+L**, every cell is
  rewritten in place (no clear, no flicker), healing anything the terminal got
  wrong. Background updates are capped at 60 frames/s (input is drawn at once).
- **Index** re-parses `workspace.yaml` only when it changes and tails
  `events.jsonl` incrementally (last 1 MiB initially) only for sessions on the
  canvas. Status is computed from those events, the in-use lock, and owned
  process state.

## Performance (measured)

`copilot-canvas bench --terminals 100` (release build, 16 logical CPUs, machine
~40 % busy): 100 real ConPTY terminals each streaming colored output at 20
lines/s, four of them subscribed like a tiled UI, plus an echo terminal.

| Measure | Result |
|---|---|
| Keystroke → updated screen (round trip through service, ConPTY, child, emulator, diff, client) | **p50 1.8 ms · p95 5.4 ms · p99 9.4 ms · max 14 ms**, 0 missed of 533 |
| Full UI frame: 100 sessions + 1,200 subagent boxes + 10 groups + 4 tiles, 200×50 | **p50 2.5–3.2 ms · p95 3.8–4.5 ms** (all zoom levels) |
| Launch 101 terminals | 2.5 s |
| Service CPU while 100 terminals stream | 14.5 % of one core |
| Service memory | ~5 MB per live terminal (emulator + scrollback); 2–6 MB with none |
| Panning a 94-session canvas, 200×55 | input→screen **≈2 ms**, 4–5 KB per frame (the dot grid hides while panning) |
| Panning while 100 sessions are live and 30 are busy (events every 50 ms, subagents spawning, permission prompts) | input→screen **≈8 ms**, UI thread ≤3 ms per frame, service updates ≤2 ms per pass |
| 300 sessions + 3,000 subagent boxes + 30 groups, 30 busy | UI thread ≈4–6 ms per frame (sidebar cached; rebuilt only when it changes), first load 15 ms (was 369 ms) |
| Typing in a session while 40 background sessions burn CPU | keystroke→screen p50 **5 ms**, p99 14 ms (257 ms / 1.26 s without the priority manager) |

The benchmark's helper processes stand in for Copilot; real Copilot processes
(Node, MCP servers, tools) are far larger than the manager and dominate
memory at 100 live sessions. Re-run on your machine:
`copilot-canvas bench --terminals 100 --seconds 15 --json bench.json`, and the
UI load tests `cargo test --release stress_probe -- --ignored --nocapture`
(synthetic busy sessions; `STRESS_SESSIONS`/`STRESS_AGENTS` scale it), `copilot-canvas bench --burn 40`
(keystroke latency with CPU-burning background sessions) and `PERF_CANVAS=<copy of a canvas.json> cargo test
--release perf_probe -- --ignored --nocapture` (panning a real canvas).
`copilot-canvas --perf` shows the same numbers live in the status bar.
The GPU client has the same probes (`bench --client gui --stress`,
`--perf-canvas FILE`, `--burn N`), `selftest --client gui --hardening`, golden
screenshots with a tolerant image diff (`selftest --client gui --golden`) and a
long-run remote soak (`selftest --remote soak`); numbers in `docs/GUI.md` §4
and `docs/REMOTE.md`.

## Safety

- Copilot history and VS Code storage are **read only** (SQLite opened with
  `mode=ro`). Canvas labels never rename Copilot sessions. The only write to
  Copilot's database is the automation enable/disable toggle you trigger.
- Configuration changes go through the Copilot CLI (`copilot plugin|mcp|skill|config …`)
  and every removal asks first. Migration backs up first and never overwrites.
- A session that is live elsewhere (CLI or VS Code) is marked ↗ and is never
  started a second time; the service rechecks the live lock (guarded against
  PID reuse) right before every launch.
- Sessions start with **`--allow-all`** by default (tools, paths and URLs
  without asking), on every launch including resumes. Set *Settings ›
  Permissions* to `ask` for the normal approval prompts, for everything or
  per group. No prompt is ever sent automatically (a first prompt only if you
  type one in the dialog).
- The pipe accepts only your user SID and local clients, and both sides prove
  knowledge of a per-start token (HMAC) so neither can be impersonated. The
  client connects with identification-only impersonation rights.
- **Remote mode** (off until you pair a client): the service listens on
  loopback only (bound exclusively), behind a private Dev Tunnel (owner
  sign-in). Each paired client has its own secret (a one-time 12-character
  pairing code shown as `ABCD-EFGH-JKMN`, which the service proves it knows
  before the client does; DPAPI on both sides; `remote revoke` drops it at
  once) and both sides prove it per connection. Over TCP the service token
  only works for your own processes. A paired client can do what you can on
  the Cloud PC: pair only your own machines (security notes in
  docs/REMOTE.md). Remote links never execute anything: URLs open at home, the
  Cloud PC's files in VS Code through its tunnel or in a read-only viewer.
- Stop is graceful (Ctrl+D, then the whole process tree after 5 s). The service
  sits in a kill-on-close job so nothing is orphaned if it dies.
- **Idle auto-stop** (*Settings › Performance › Stop idle sessions*: never /
  30 / 60 / 120 / 240 (default) min) gracefully stops a canvas-started Copilot
  session only when it is idle (not working, waiting on you, waiting for review
  or looping on a schedule),
  not shown in a pane, and has had no output, typing or history events for that
  long. Each one frees its ~30 helper processes; the conversation stays
  resumable with a double-click.
- **Passive sessions** treat event data and messages as untrusted data, never
  instructions (the run prompt quotes them as such); their templates are
  read-only by default (report, don't push or comment). Event chains stop after
  3 hops, a session's own events never trigger it, and runs are capped per day.
- **Automations**: the canvas only reads Task Scheduler, launcher logs and
  reports; it changes tasks when you run, enable or disable one, or `apply`
  (preview first, backups kept). The doctor follows each automation's owner
  docs and never sends messages, touches pull requests, IcM, flights or
  production, or disables an automation to hide a failure.
- If the client ever crashes, the panic and backtrace are appended to
  `~\.copilot-canvas\crash.log` and the client exits cleanly; sessions keep
  running in the service, so starting it again reattaches.

## Commands

```text
copilot-canvas [--demo] [--perf] [--cwd DIR] [--canvas FILE] [--instance NAME] [--home DIR] [--remote NAME]
copilot-canvas --gui | --tui [options]
                                      the GPU client (copilot-canvas-gui.exe) or the TUI, whatever the
                                      client setting says
copilot-canvas config [client|agent|engine [VALUE]]
                                      machine settings: client tui|gui|auto, agent copilot|opencode|
                                      claude|auto, engine ghostty|alacritty|auto
copilot-canvas record … · corpus check|bless|diff|list
                                      golden terminal corpus (corpus/README.md)
copilot-canvas doctor                 environment checks (each agent CLI and its history, the default
                                      agent, terminal engine, Git, ConPTY, service, VS Code)
copilot-canvas service status|stop    stop ends the service's sessions (gracefully)
copilot-canvas bench [--terminals N] [--seconds S] [--json FILE]
copilot-canvas bench --client gui     the GPU client's latency, frame cost, idle CPU, memory, first paint
copilot-canvas selftest               end-to-end UI test in a hidden console (demo shells only)
copilot-canvas selftest --client gui  the same 32 steps against the GPU client (over its test hooks)
copilot-canvas selftest --client gui --parity
                                      one GUI check per row of the parity matrix (docs/PARITY.md)
copilot-canvas kitty-demo             print a small test image (iTerm2 inline image; the GPU client shows it)
copilot-canvas selftest agent [--agent copilot|opencode|claude]
                                      real check with an agent CLI (default: the detected one); never
                                      submits a prompt: imports its recent sessions into a temporary
                                      canvas, loads its plugin/MCP/skill views, starts a session with
                                      allow-all and the initial default model (Opus 5.5, its own id),
                                      types (and erases) text, stops it gracefully, and
                                      removes a test session only if it holds no prompt. Copilot also
                                      syncs VS Code sessions and previews migration (applies nothing).
                                      Claude Code runs in a throw-away CLAUDE_CONFIG_DIR.
copilot-canvas selftest copilot       same as selftest agent --agent copilot
copilot-canvas selftest agent --paste [--agent copilot|opencode|claude]
                                      pastes multi-line text (a blank line, indentation, a trailing line
                                      break) into the real agent's input box through the canvas and
                                      checks it arrived as lines and nothing was sent; erases it again
                                      (OpenCode runs with a free model as every default, Claude Code in a
                                      throw-away CLAUDE_CONFIG_DIR; docs/AGENTS.md#pasting)
copilot-canvas selftest agent --agent opencode --prompt [--model ID] [--client tui|gui] [--steps LIST]
                                      OpenCode through the canvas with tiny prompts to a free model only
                                      (ids ending in -free; default opencode/deepseek-v4.1-flash-free):
                                      re-keying, working → done · review → idle, wheel, git push event,
                                      subagent box, question and permission (needs you, rejected), resume,
                                      a headless passive run; deletes every session it created
                                      (docs/AGENTS.md)
copilot-canvas selftest --remote loopback|faults|paired
                                      remote mode: the UI test over TCP; drops, cut frames and delay;
                                      pairing, a Dev Tunnel stand-in, revocation (docs/REMOTE.md)
copilot-canvas remote tunnel-setup|pair|clients|revoke    on the Cloud PC (docs/REMOTE.md)
copilot-canvas remote add|servers|forget                  at home; then copilot-canvas --remote NAME
cargo test                            unit tests
```

## Limitations

- Sessions already running in another terminal can't be attached to (no
  Copilot API for that); open them where they run.
- Subagents run inside their parent's terminal; their boxes show status and
  their transcript (Inspect). Only OpenCode's child sessions resume on their
  own (as a linked subsession, once the parent is stopped).
- VS Code sync and migration are Copilot-only (VS Code's agent sessions are
  Copilot CLI sessions); OpenCode and Claude Code sessions are imported from
  their own history. Automations (`data.db`), `/every` loops and the
  plugin/skill install actions are Copilot features too. VS Code lists each agent session in several workspace
  caches; each is assigned to the workspace whose folder best matches the
  session's working directory.
- "Done · review" relies on Copilot's events; when no idle event is written, a
  turn end followed by 4 s of silence with no open tool calls counts as idle.
- The TUI's grid canvas is character-based (three zoom levels), not
  pixel-smooth, and draws no inline images (as in Windows Terminal); the GPU
  client (`--gui`) draws Kitty graphics and iTerm2 inline PNG images. OSC 52
  copy is forwarded to your terminal.
- Windows only (ConPTY, named pipes, job objects).
- A terminal selection covers the rows in view (dragging does not scroll).
  On full-screen views every row is copied as its own line: such programs
  wrap their paragraphs themselves, so a paragraph copied from Copilot or
  OpenCode keeps their line breaks. cmd.exe has no multi-line input: a
  pasted line break ends its line (docs/AGENTS.md#pasting).
- Stopping a session sends the agent Ctrl+D (twice), then Ctrl+C, and ends its
  process tree after 5 s; a session the canvas stopped always shows as stopped.
  OpenCode 1.19 sometimes quits its screen but leaves its process running (on a
  busy machine, or while one of its own background requests is pending); the
  canvas ends such a process 3 s after its screen closed.
- OpenCode names a session on its first prompt: the box learns that `ses_` id
  within a few seconds and follows it from then on; a TUI that starts another
  session (`/new`) keeps the box on its first one. OpenCode sessions open in an
  OpenCode the canvas did not start are not shown as open elsewhere. See
  [docs/AGENTS.md](docs/AGENTS.md#known-limitations).
- Automations are read from Copilot's local database; creating or editing
  their prompt/schedule is done in VS Code or the Copilot app.
- Upgrading: terminal fixes live in the service. When a newer build starts
  and finds an older service, it restarts an idle one automatically; if it
  still runs sessions it asks first. If you keep it, a gold **⟳ Update
  service** toolbar button stays until you click it: it restarts the service
  and resumes every terminal that was running (Copilot continues the same
  conversation; a turn in progress is interrupted). Older services launched
  Copilot with `--mouse=off`, which Copilot saves as `"mouse": false` (the wheel
  then only pages); the update turns it back on via `copilot config mouse true`.
  With an older protocol the restart is required.
- Scheduled tasks are registered with your interactive token (no password
  stored), so they run only while you are signed in. Schedules support daily,
  weekly, every-N and logon triggers.
- Passive schedules run only while the service runs (start it at logon with
  `copilot-canvas service start` from a scheduled task). Rollover is by run count, not context size. Copilot plugin hooks
  don't fire from `--plugin-dir`, so PR activity is detected from session
  history (a few seconds later) rather than hooks.
- `--perf` shows frames per second, draw time and output-to-screen latency in
  the status bar.
