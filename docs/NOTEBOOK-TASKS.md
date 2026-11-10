# Notebook, Tasks and Spaces: implementation spec

Status: **implemented** on 2026-10-31 (§1 to §6); the iPhone app's
Notebook and Tasks places (viewing and editing) on 2026-11-02 (§7). User
documentation: README (*Notebook*, *Todos*, the feature reference),
docs/GUI.md, docs/BACKEND.md, docs/AGENT-PROVIDERS.md, docs/REMOTE.md,
docs/IOS-STATUS.md, ios/README.md.

Approved by the owner on 2026-10-31 (every recommended option of the
visual plan, `My Notes/Canvas App/Notebook and Tasks - visual plan.html`).
This file is the contract the implementation follows; keep it current when a
detail changes. The text below describes what was built; *As built* lists
where that differs from the first plan.

Decisions (all "recommended"):

| # | Decision |
|---|---|
| 1 | One notebook root folder per canvas, managed by the canvas, on the service's machine (default `<home>/notebook`); other folders (group folders elsewhere, Obsidian folders) are mounted, never moved. |
| 2 | Notes are classified by **audience** (for you / for agents); **author** is a secondary mark. Default view: everything, AI notes dimmed. |
| 3 | Editor: a native multi-line source editor in `canvas-core` (cells; TUI, GUI, remote), with a Live Preview mode. |
| 4 | Todos: a JSON store owned by the service (single writer), mirrored to Markdown. |
| 5 | "Blocker" is a flag (`blocker`) plus `blocked` reason, not a type. Types: bug, feature, improvement, optimization, chore, docs, research. |
| 6 | Agents `add` todos when the user asked, `propose` when it is their own idea; a board's policy can force every agent-created todo to Proposed. |
| 7 | Sub-boards are swimlanes inside their parent board (sharing its columns). |
| 8 | The TODO bot can move onto the store (`todo ready/claim/submit`); TODOs.md can be a generated mirror. The live bot script outside this repository is not changed by this work. |
| 9 | Reports from a session in a group with a folder are also filed into `<group>/human-outputs/reports/`. |
| 10 | HTML notes: scripts off by default; allowed per file (remembered per canvas), offline only (no network, no CDNs). |
| 11 | Order: spaces foundation, then Notebook and Tasks in parallel. |

General rules for every part:

- Features exist in both clients (TUI and GUI) through `canvas-core`, and work in
  remote mode (everything on the machine goes through the backend RPC or the
  service). The GUI may add pixel polish; HTML rendering is GUI-only with a
  formatted-text fallback in the TUI (as reports do).
- Viewing never starts or resumes a session. Only explicit actions start.
- `canvas-model`, `canvas-common`, `canvas-termview`, `canvas-mobile` must stay
  portable (they are built and tested on macOS in CI): no `windows` crate there.
- Backward compatibility: new JSON fields use `#[serde(default, skip_serializing_if…)]`.
  Never add a `NodeKind`. Append new enum variants of wire types at the end.
- Rust 2024, `cargo clippy --workspace --all-targets -- -D warnings` clean, no
  rustfmt (the repository does not use it; match the surrounding style: long
  lines are fine), concise doc comments, no emojis in code.
- Glyphs used in cells must be single-width: `◫` canvas, `✎` notes, `☑` todos,
  `●` for you, `◇` for agents, `‼` blocker, `⚑` blocked, `⟳` working session.

## As built: changes from the first plan

| Where | Planned | Built (and why) |
|---|---|---|
| §1 keys | `Alt+←` / `Alt+→` | also `Alt+,` / `Alt+.` (Windows Terminal keeps Alt+arrows and Ctrl+Alt+← for its panes) and the GUI's mouse back / forward buttons (also while a terminal has the keyboard) |
| §1 help | one help | `?` shows the keys of the space you are in |
| §1 group menu | four new items | *Link notes folder…* replaces *Change group folder…*; the chips shrink by importance when the title row is narrow |
| §2.2 folders | any path component | the deepest of `ai-docs`, `.canvas`, `human-outputs` decides; `.git` is internal too |
| §2.3 `NotebookList` | `{ dir }` | `{ dir, root: Option<String> }`: entries are classified relative to the explorer root (`None`: `dir`); a root inside `<home>/canvas-agents` is the canvas-wide one |
| §2.3 limits | 5,000 entries, 4 MiB, 16 MiB | also: `NoteRevs` at most 500 paths; `NoteSearch` at most 500 hits, 20,000 files, 256 MiB, 10 s, files of 2 MiB, symlinks skipped, a 60 s request timeout (not a quick request); `NotebookList` reads 50,000 names |
| §2.4 HTML notes | the reading look | rendered Markdown gets `report_html::NOTEBOOK`; an HTML note that is a whole document (`<html>`, `<head>` or `<body>`) keeps its author's look (only the policy and its title are put first), a fragment gets the reading look (`report_html::notes::page`) |
| §2.5 filter key | `Ctrl+Shift+H` | `Alt+H` (terminals cannot tell Ctrl+Shift+H apart) |
| §2.5 quick capture | `Ctrl+Shift+N` | `Alt+N`, in every space outside terminals and dialogs (the GUI keeps Ctrl+Shift+N for a new window) |
| §2.6 agent env | `ATELIER_AGENT` | plus `ATELIER_SESSION_AGENT` (same value): `ATELIER_AGENT` also chooses the agent for new sessions, and the marker keeps a canvas started inside a session from taking the session's agent as that choice |
| §2.6 report filing | provenance with the agent | `agent`: the session's agent from the routing (`Routing::agents`, session id → agent, sent by windows of this build; none from older ones), else a passive spec's; `created` is the report's time, `report` its id |
| §2.6 `note new` agent | from `ATELIER_AGENT` | from `ATELIER_SESSION_AGENT` (set with it for sessions), else the routing: `ATELIER_AGENT` alone may be a choice of agent for new sessions, not the writer |
| §2.3 writes | containment in `roots` | also, on Windows, no name below the root that Windows reads as another (trailing dot or space, `:` streams, device names, `GIT~1`), no link or junction leading out of every root, no-replace renames (`MoveFileExW`), retries while another program holds a file, and never a save over a file that is not UTF-8 (read lossily) |
| §3.1 claims | `claim` for oneself | `TodoOp::Claim { id, force, session }`: people and bots claim for a session of the canvas (`todo claim ID --session NAME\|ID`, *Claim for…*); agents only for themselves |
| §3.2 refusals | a notice | `ServiceMsg::Error { id: None, message: "Todo change refused: …" }` to that window, then the book as it is (its optimistic change is undone) |
| §3.2 catalogs | — | `bus::upgrade_catalog`: an existing group catalog without `todo.*` events gains the seven default ones (lines inserted only), on `EnsureCatalog` and each group passive run |
| §3.2 log | — | `log.jsonl` moves to `log.jsonl.old` beyond 5 MB |
| §3.1 policy | the listed rules | also: agents never change the status of (or close) a todo another live session claimed, never mark a proposal done or reopen a declined one, and add swimlanes only to boards of their group; per todo at most 100 relations and 32 KB of log text (the newest kept); ids stop at 999,999,999 per prefix (an error, never a 10-digit id) |
| §3.2 store | atomic writes | also flushed before the rename; a book that cannot be read (or that a newer version wrote) is never written over (changes refused); a change that grows the book beyond 16 MB is refused (clients get it whole, in one frame); a request read twice from the inbox is applied once; the mirror first copies a file it did not write to `FILE.bak` |
| §3.3 CLI | the listed verbs | also `todo delete ID`, `todo mirror --set FILE\|off` (the service's mirror; `--out FILE` writes a snapshot), `todo claim --session`, `todo update --label -L` (removes), `todo accept ID --status S`, `todo board unlink B --group G`, `todo board delete B [--with-todos]` (refused while it holds todos unless `--with-todos`, which deletes them) |
| §3.3 default board | the first board linked to the caller's group | that, and outside a group (you, canvas-wide agents, automations) the canvas's only board when it has exactly one |
| §3.4 keys | arrows, Enter, Space, N, A, D, Del | also `E` (edit the body), `R` (rename), `F` (every board), `t` (sidebar: the next type filter), Ctrl+arrows pan (arrows pan only while nothing is selected); the board menu adds *New swimlane…*; the details add *Claim for…*, *Release*, *Done*, *Cancel*, *Reopen* |
| §3.4 optimistic edits | applied at once, the service's next book replaces them | a book the service sends before it has a window's change (its broadcast after an earlier change, a bus pass that read the book a moment before) gets the change put back on top (`tasks::book::receive`; the service applies a window's changes in order, one revision each) until a book has it, the service refuses it (its error undoes it at once) or 5 s pass; the iPhone core does the same (`canvas-mobile` `todos::InFlight`); the todo canvas's layout is made once per book and moved when only the pan changes |
| §4 web pane | one view | two: the plain one never runs scripts; notes allowed to run theirs get a second in its own in-private profile, unloaded when hidden |
| §4 origin | the note's folder | the origin is mapped to the explorer root the note is in (the notebook root, a group folder outside it, a mount; else its folder), so relative links resolve as on disk and open other notes in the app; only the note's own folder and the folders below it are served (`webpane::classify`) |
| §4 hardening | CSP, 403 | also `nosniff`, `Cache-Control: no-store`, `Referrer-Policy: no-referrer`, `X-DNS-Prefetch-Control: off`; browser arguments for a dead proxy (`127.0.0.1:9`, loopback included: `--proxy-bypass-list=<-loopback>`), no DNS prefetching and WebRTC only through the proxy; SmartScreen off; external drops off; `GET` only, documents only the note itself; workers' requests filtered too where the runtime can; at most 256 requests waiting per window (503 beyond), a replaced page's answered at once; the sanitizer writes `<` in kept CSS as `\3C ` (no markup through `<svg><style>`); in the scripts view one app link per click into it |
| §4 keys | the listed keys | also `Alt+,` / `Alt+.`, `Ctrl+Shift+Tab`, `Ctrl+N`, `Ctrl+S`, `Ctrl+F`, `Alt+N`; Esc gives the keyboard back from a note (closes a report) |
| §4 print | WebView2's print UI | that (`ShowPrintUI`); a runtime without it saves `<Downloads>\<title>.pdf` (never over a file: the name is claimed by creating it) |
| §7 mobile | may expose the data | `canvas-mobile` exposes the boards and the notebook with changes (2026-11-02: note sources saved on their revision, new notes and folders, renames, the trash; todo fields, the checklist, reopen, cancel, release); the iPhone app has Notebook and Tasks places (§7); `canvas-mock` serves a demo notebook and book |
| §3.1 ops | check, add check item | also `edit_check` (rewords an item, its tick kept) and `remove_check` (people only), for the phone's checklist editing; the desktop's details still offer toggle and add |
| §2.5 new notes | the text in `canvas-core` | `notebook::your_new_note` and `notebook::typed_name` in `canvas-model`, shared by the desktop and the phone |

## 1. Spaces (canvas-core: `spaces.rs`, app/ui/events wiring)

A window shows one of three **spaces** (`spaces::Space { Canvas, Notebook, Tasks }`,
`App::space`, kept in the window's prefs): the session canvas (today), the
Notebook, the Tasks boards. The frame stays: toolbar, activity bar, sidebar,
main area, right pane, status bar.

- **Toolbar**: tabs `◫ Canvas`, `✎ Notebook`, `☑ Tasks` at its left (compact
  `◫ ✎ ☑` when narrow); a click switches (`Hit::Toolbar(Action::Space(..))`).
- **Keys** (not while a terminal has the keyboard): `Ctrl+1/2/3` switch spaces;
  `Alt+←` / `Alt+→` (also `Alt+,` / `Alt+.`, and the GUI's mouse back /
  forward buttons, those also from a terminal) go back / forward in the
  navigation history; `Ctrl+P` opens **Go to** (one search over groups,
  sessions, notes, todos); `Alt+N` quick capture (§2.5); `?` shows the
  space's own keys.
- **Activity bar**: slot 1 is "this space's sidebar": `▤ SESSIONS` (Canvas),
  `✎ EXPLORER` (Notebook), `☑ TASKS` (Tasks). `Alt+1` keeps selecting slot 1;
  slots 2–9 (Automations … Reports) are unchanged in every space.
- **Main area** (the canvas rect): Canvas draws the session canvas; Notebook
  and Tasks call their module (`notebook::draw_main`, `tasks::draw_main`). In
  the GPU client the main area of Notebook/Tasks is drawn from cells (no
  `CanvasModel`).
- **Right pane**: terminals when tiles are shown; else `right_detail`; else the
  space's own panel (`notebook::draw_info`, `tasks::draw_info`). New
  `RightDetail::Note(path)` and `RightDetail::Todo(id)` take the pane like the
  others.
- **Focus**: `Focus::Canvas` means "the main area of the current space"; keys
  and mouse there go to the space's module when the space is not Canvas.
- **Navigation history** (`spaces::History`, per window, in memory, 100
  entries): entries are `(Space, NavTarget)` with
  `NavTarget { None, Group(id), Node(id), Note(path), Folder(path), Board(id), Todo(id) }`.
  Every jump between spaces (chips, badges, Go to, links) pushes an entry.
- **Go to** (`Modal::GoTo`): fuzzy (subsequence, case-insensitive) match over
  providers: groups and sessions (doc), `notebook::goto_items(app)`,
  `tasks::goto_items(app)`. Enter jumps (never starts a session).
- **Module hooks** (stubs in Wave 1, filled in Wave 2), each module owns its
  directory and these entry points:
  - `State` (in `App::notebook` / `App::tasks`), `Action` enum
    (`Action::Notebook(..)` / `Action::Tasks(..)`, dispatched to `run`),
    `Hit` enum (`Hit::Notebook(..)` / `Hit::Tasks(..)`, dispatched to `click`),
    `Dialog` (`Modal::Notebook(Box<..>)` / `Modal::Tasks(Box<..>)`, with
    `draw_dialog`, `dialog_key`, `dialog_mouse`).
  - `draw_side`, `draw_main`, `draw_info`, `draw_detail` (for RightDetail),
    `on_key` (bool handled), `on_mouse`, `on_text` (IME/paste), `tick`
    (every UI pass: autosave, polling), `goto_items`, `open(app, NavTarget)`.
  - Notebook only: `group_has_notes(app, group)`; Tasks only:
    `group_counts(app, group) -> Option<TodoCounts { open, in_progress, proposed }>`,
    `session_badge(app, node) -> Option<String>` (the todo a session claimed).
- **Main canvas links** (spaces foundation owns them):
  - Group title chips: `✎ Notes` replaces `◧ AI docs` + `◨ Outputs` (opens the
    Notebook at the group's folder; `✎ Set folder…` before one is chosen);
    `☑ 4 · 2▸ · 1◇` (open · in progress · proposed; lit gold while proposals
    wait; `☑ +` when no board is linked, which offers to create or link one).
    Compact forms: `✎`, `☑4`. Keep `⟲` passive chips.
  - Session boxes show `☑ DEC-031` when the session claimed a todo
    (`BoxModel::badge` in the GPU model, a cell badge in the TUI); a click opens
    the card in Tasks.
  - Group menu: *Open notes*, *Open todo board*, *Link notes folder…*,
    *Link todo boards…* (the latter two call module actions; *Link notes
    folder…* replaces *Change group folder…*).
  - Group details panel: NOTES and TODOS sections with the same links.

## 2. Notebook

### 2.1 Folders

- `Doc::notebook_root: Option<String>` (shared field; `None`: `<home>/notebook`
  on the service's machine), `Doc::notebook_mounts: Vec<String>` (extra
  folders), `Doc::notebook_scripts: Vec<String>` (HTML files allowed to run
  scripts). All three in `shared_fields!`.
- `Request::NotebookInfo` resolves and creates the root (with `Groups/`,
  `Inbox/`, `Todos/`) and answers it with the canvas-wide folder
  (`<home>/canvas-agents/<canvas id>`) and the service's home.
- New groups get the suggested folder `<root>/Groups/<Group name>` (was
  `<home>/groups/<slug>`). Existing group folders stay where they are; those
  outside the root are mounted.
- The explorer's roots: **Notebook** (the root), each group folder outside it
  (`◫ <group>`), **Canvas (canvas-wide)** (the canvas-wide folder, AI),
  extra mounts, and a virtual **Reports** section (the inbox; opening one uses
  the existing report viewer). A folder that is a group's folder shows `◫`
  (and `☑` when that group has a linked board).

### 2.2 Provenance (`canvas_model::notebook`)

Frontmatter: a block of flat `key: value` lines between `---` lines at the
very start of a Markdown file (values may be quoted; lists kept as raw text).
Unknown keys and their order are preserved when the app rewrites fields. HTML
files carry the same keys as `<meta name="canvas-KEY" content="VALUE">`.

Keys: `audience` (`human` | `ai`; also accept `agent`→ai, `user`/`you`→human),
`author` (`human` | `ai` | `mixed`), `agent`, `source_session`, `created`,
`reviewed` (`true`/`false`), `title`.

`classify(rel_path, meta)` (rel_path relative to its root, `/` separators):

1. `audience` from the metadata → source **frontmatter**.
2. Else by folder (the deepest of these path components decides): `ai-docs` → ai; `.canvas` → ai;
   `human-outputs` → human; the canvas-wide root → ai; the Reports section →
   human → source **folder**.
3. Else `.html`/`.htm` → human → source **file type**.
4. Else human → source **default**.

`author`: metadata, else ai inside `ai-docs`, `.canvas`, `human-outputs`,
the canvas-wide root and the Reports section, else human. `inferred` = source is not frontmatter.
`internal` = inside `.canvas` (except `.canvas/passive/<slug>/{rules,state,log}.md`,
shown as passive memory), names starting with `.`, `events.yaml`,
`canvas-group.instructions.md`, and the `.trash` and `.git` folders. Title: metadata
`title`, else the first `# ` heading / HTML `<title>`/`<h1>`, else the file stem.
Kinds: markdown (`.md`, `.markdown`), html (`.html`, `.htm`), text (other
known text extensions), image (`png jpg jpeg gif svg webp`), other.

The app writes `author: mixed` into a note whose author is ai with your
first change to it (frontmatter only; HTML gets the meta tag; the line joins
that change's undo step, so undoing it takes the line back too), and
`reviewed: true` on *Mark reviewed*.

### 2.3 Backend requests (`rpc.rs`, `canvas-agents/src/backend.rs`)

Appended at the end of `Request` / `Response`:

| Request | Answer | Notes |
|---|---|---|
| `NotebookInfo { root: Option<String>, canvas: Option<String> }` | `Response::NotebookInfo(notebook::Info { root, canvas_dir, home })` | creates the root and its three folders |
| `NotebookList { dir: String, root: Option<String> }` | `Response::NoteEntries(Vec<notebook::Entry>)` | entries with kind, size, modified, `Provenance`, title (metadata read from the first 8 KiB), classified relative to `root` (`None`: `dir`; a root inside `<home>/canvas-agents` is the canvas-wide one); folders first; at most 5,000 (of 50,000 names read) |
| `NoteRead { path }` | `Response::Note(notebook::NoteText { text, rev })` | at most 4 MiB, UTF-8 (lossy) |
| `NoteWrite { path, text, base: Option<String>, roots: Vec<String> }` | `Response::NoteSaved { rev }` or `Response::NoteConflict(NoteText)` | `base: None` means the file must not exist; atomic write; creates parent folders |
| `NoteRevs { paths }` | `Response::NoteRevs(Vec<Option<String>>)` | change polling; at most 500 paths |
| `ReadBytes { path, max_bytes }` | `Response::Bytes(Vec<u8>)` (base64 in JSON) | at most 16 MiB |
| `NoteMkdir { path, roots }` | `Done` | |
| `NoteRename { from, to, roots }` | `Done` | refuses to overwrite |
| `NoteTrash { path, root }` | `Done` | moves into `<root>/.trash/<ms>-<name>` |
| `NoteSearch { roots, query, limit }` | `Response::NoteHits(Vec<notebook::Hit { path, line, text }>)` | case-insensitive names and contents of text files, skipping `.trash`, `.git`, `node_modules` (and links); at most 500 hits, 20,000 files, 256 MiB, 10 s, text files of 2 MiB; a 60 s request timeout |

`rev` = lowercase hex of the first 8 bytes of SHA-256 of the file's bytes.
Every request refuses relative paths and `.` or `..` components; write
requests also refuse paths inside `.git` and paths that are not strictly
inside one of `roots`. Every request gets a backend contract
test (local and remote answer alike).

### 2.4 Rendering

- Markdown → HTML: `canvas_model::notebook::markdown_html(text)` (pulldown-cmark
  `html` feature: tables, task lists, strikethrough, footnotes; frontmatter
  stripped; `[[Note]]` turned into links `#note:Note`, todo ids like `DEC-031`
  into `#todo:DEC-031`), then `report_html::document(.., &report_html::NOTEBOOK)`.
  `NOTEBOOK` is a reading look (dark, the app's palette, proportional type,
  max width ~80ch, good headings, code, tables, quotes, task lists).
- HTML notes (`report_html::notes::page`): sanitized as reports are, plus
  links, pictures, media and style sheets that stay on the notes origin; a
  whole document (`<html>`, `<head>` or `<body>`) keeps its author's look
  (its `<html>`/`<body>` attributes and styles; the policy and its title go
  first), a fragment gets `NOTEBOOK` with its own styles after it.
- GUI: the core publishes an HTML pane (§4) for the reader in Reading mode;
  the TUI (and the GUI when WebView2 is missing) uses `richtext`.

### 2.5 Notebook space UI (canvas-core `notebook/`)

- **Explorer** (sidebar): lazy tree of the roots (background `NotebookList`),
  `● / ◇` markers, AI notes dimmed and italic in the GUI (dim in the TUI),
  `(inferred)` only in the info panel. The **Human | All | AI** filter (click
  the segment; key `Alt+H` cycles; shown hidden count "N AI hidden");
  folders that hold only hidden notes disappear. *Show internal files* toggle.
  Actions: new note (`Ctrl+N`), new folder, rename (`F2`), move (*Move to…*),
  delete to trash (`Del`, asks), copy path, open externally, mark reviewed,
  *Show group on canvas*. `/` filters names.
- **Reader** (main): tabs (at most 20; `Ctrl+W` closes, `Ctrl+Tab` /
  `Ctrl+Shift+Tab` or `Ctrl+PgDn` / `Ctrl+PgUp` step), a header with
  breadcrumb, mode (`Rendered | Source | Live`), provenance pill
  ("● For you · ◇ written by OpenCode · session X · 2 h ago", *View session*,
  *Mark reviewed*), buttons *Print / PDF* (GUI), *Open externally*,
  *Ask agent to revise…* (sends the path to a chosen session of the group
  with `ClientMsg` messaging, typed in when idle, like `send`).
- **Editing**: `Ctrl+E` toggles Rendered ⇄ Source; *Live* is the editor's live
  preview. `Ctrl+S` saves; autosave 1 s after typing stops; the open notes'
  revisions are polled every 2 s: a clean buffer reloads silently, a dirty one
  shows the conflict banner (*Compare*, *Keep mine*, *Take theirs*). A note
  whose file is gone shows a banner and its next save writes it anew (a
  write that needs no file); a rename or move waits for a save on its way,
  and no save starts until it is done. Quitting writes every unsaved note
  (over a save still on its way); one that cannot be written (an unsettled
  conflict, a refused write) is kept beside it as
  `<name> (unsaved <yyyy-mm-dd HHMM>).<ext>`.
  `[[` opens note-name completion. Links: `[[Note]]` opens the note,
  `DEC-031` opens the todo, http(s) asks before the browser.
- **Info panel** (right pane): outline, linked (group, boards, todo ids found
  in the text), backlinks (`NoteSearch` for `[[stem]]`), properties
  (frontmatter), provenance (with *inferred*), *View session*.
- **Quick capture**: `Alt+N` in every space outside terminals and dialogs
  (the GUI keeps `Ctrl+Shift+N` for a new window), or `+ Capture`, creates
  `<root>/Inbox/<yyyy-mm-dd HHMM>.md` (`-2`, `-3` within a minute; audience
  human, author human) and opens it in Source mode.
- **Settings** (Settings view): *Notebook folder*, *Notebook mounts…*.

### 2.6 Agents and the notebook

- Group instructions (`prepare_group_folders`, agent-neutral wording, replaces
  "This Copilot session"):

  ```
  # Canvas group: {name}

  You are working in a session of the "{name}" group in Atelier.

  ## Notes (the canvas notebook)
  - Notes for other agents (plans, research, investigation logs, context): `{ai_docs}`
  - Deliverables for the user (reports, summaries, final documents; prefer one self-contained HTML file when layout matters): `{outputs}`
  - Start Markdown notes with front matter (`audience: ai` or `audience: human`, `author: ai`, `source_session: <ATELIER_SESSION>`); `atelier note new` writes it for you.
  - Before substantial work, read `{ai_docs}` (GOALS.md first) for context left by other sessions.

  ## Todos
  - The canvas keeps todo boards. Run `atelier todo prime` to see the boards linked to this group and how to use them; change todos only with `atelier todo …`.
  ```

- `atelier note path [--audience ai|human]`: where to write (the
  caller's group: `ai-docs` / `human-outputs`; no group: `<root>/Inbox`).
- `atelier note new --title T (--md FILE|- | --html FILE|-) [--audience ai|human] [--folder REL] [--name FILE]`:
  writes the note with provenance (`author: ai`, `source_session`, `agent`
  from `ATELIER_SESSION_AGENT`, else the routing, `created`) and prints its path. Default
  audience ai. Never overwrites (adds `-2`, `-3`).
- `atelier note list [--audience ai|human] [--format json]`: the
  caller's group notes.
- The routing the windows send gains `notebook_root` (the canvas setting;
  absent for the default) so the CLI and the service know it, and `agents`
  (session id → its agent); sessions and
  passive runs get `ATELIER_AGENT` (copilot, opencode, claude) and
  `ATELIER_SESSION_AGENT` with the same value: the marker tells
  `agents::agent_from_env` that the variable names the session's agent and is
  no choice of agent for new sessions (a canvas started inside the session
  keeps its own default).
- Skill `canvas-notes` (`notes_cli::SKILL`).
- Reports (§ decision 9): when the service stores a Markdown or HTML report
  from a session whose group has a folder, it also writes
  `<folder>/human-outputs/reports/<yyyy-mm-dd>-<slug>.<md|html>` with
  provenance (audience human, author ai, source_session, created, `report`:
  the report id; `agent` from the routing). Never overwrites; failures
  are logged and never refuse the report.

## 3. Tasks

### 3.1 Model (`canvas_model::todos`)

```rust
pub struct TodoBook { pub version: u32, pub rev: u64, pub boards: Vec<Board>, pub todos: Vec<Todo>, pub seq: BTreeMap<String, u32>, pub settings: TodoSettings }
pub struct TodoSettings { pub mirror: Option<String> }              // Markdown mirror path
pub struct Board { pub id: String, pub name: String, pub prefix: String, pub parent: Option<String>,
    pub x: i32, pub y: i32, pub w: i32, pub h: i32, pub color: u8, pub wip: Option<u32>,
    pub statuses: StatusNames, pub canvas_groups: Vec<String>, pub notes: Option<String>,
    pub policy: AgentPolicy, pub created_by: Actor, pub created_ms: u64 }
pub struct AgentPolicy { pub agent_adds: AddPolicy /* Direct | Proposed */, pub agents_may_close: bool }
pub enum Status { Proposed, Backlog, Todo, InProgress, InReview, Done, Canceled }   // fixed categories, in this order
pub struct StatusNames { /* optional display name per status */ }
pub enum Resolution { Done, WontDo, Duplicate, Declined, Obsolete }
pub enum TodoType { Bug, Feature, Improvement, Optimization, Chore, Docs, Research }
pub enum Priority { Urgent, High, Medium, Low, None }
pub struct Todo { pub id: String, pub title: String, pub kind: TodoType, pub status: Status,
    pub resolution: Option<Resolution>, pub priority: Priority, pub blocked: Option<String>, pub blocker: bool,
    pub board: String, pub parent: Option<String>, pub rank: String, pub body: String, pub note: Option<String>,
    pub labels: Vec<String>, pub checklist: Vec<CheckItem>, pub relations: Vec<Relation>, pub links: Vec<Link>,
    pub log: Vec<LogEntry>, pub proposal: Option<Proposal>, pub created_by: Actor, pub claimed_by: Option<Claim>,
    pub created_ms: u64, pub updated_ms: u64, pub done_ms: Option<u64>, pub rev: u64 }
pub enum RelKind { BlockedBy, Related, DiscoveredFrom, DuplicateOf }
pub enum LinkKind { Session, Pr, Note, Report, File, Url }
pub struct Actor { pub kind: ActorKind /* Human | Agent | Bot */, pub session: Option<String>, pub name: Option<String> }
```

(Exact field names may grow; serde `snake_case`, unknown-safe defaults.)

- A todo's board may be a sub-board (`parent` set): it is drawn in that
  swimlane of the parent board. Sub-boards share the parent's prefix.
- Ids: `<PREFIX>-<seq, at least 3 digits>`, assigned by the book, never
  reused. Prefix: 2–5 uppercase letters, by default from the board name
  (`Decomps` → `DEC`), unique among top-level boards.
- Rank: fractional index strings (`rank_between(a, b)`), order within a column.
- `blocked(todo)` = a `blocked` reason, or a `BlockedBy` relation to a todo
  that is not Done/Canceled. `ready` = status Todo, not blocked, not claimed.
  Sorted by priority, then rank.
- **Operations**: one serde-tagged `TodoOp` enum applied by
  `TodoBook::apply(op, &Actor, &Ctx) -> Result<Applied, String>` (`Ctx`: now,
  the actor's group, the canvas groups' names; `Applied`: changed ids, a
  message, bus events to emit). Used by the service for CLI requests and window
  actions alike, and by windows optimistically. Ops: add/update/move/link/delete
  board; add (with `propose`), update, move (status/board/rank), accept,
  decline, claim (`TodoOp::Claim { id, force, session }`: `session` names the
  session a person or bot claims it for; the stored `Claim { by: Actor, ms }`),
  release, note (append to `log`), check/add check item,
  link/unlink, relate/unrelate, submit, done, cancel, reopen, delete, set
  mirror, import, edit/remove check item (`edit_check`, `remove_check`:
  people only; appended for the phone).
- **Policy**: humans (and the TODO bot, which runs without a session) may do
  everything. Agents may add (status Proposed when the board's policy says so,
  else Backlog or the asked Backlog/Todo), propose, update fields, claim (only
  for themselves) and release their own claims, note, check, link, relate,
  submit (→ In review), move between Backlog/Todo/In progress/In review, and
  create boards (linked to their group) or link a board to their own group;
  they may not accept, decline or delete todos, change, move or delete boards, set the
  mirror, import, or close (Done/Canceled) and reopen unless
  `agents_may_close` (never cancel a proposal). A claim by another live
  session is refused unless a human forces it; a claim of a session that is
  gone or stopped may be taken over by anyone. Claiming moves Backlog and Todo
  to In progress; releasing In progress moves it back to Todo. WIP limits
  are soft (a warning).
- Import: `import_markdown(text, board, prefix)` reads the owner's TODOs.md
  format (`- [ ] **WI-019: [[WI-019 - Title]]** - text`, `- [x] … Done DATE in \`hash\``,
  `(bot: blocked DATE: reason …)`, `### WI-nnn - Title` sections → body,
  `## Relationships` bullets → `Related` relations and a body line), keeping
  the ids and advancing the prefix's sequence.
- Mirror: `mirror_markdown(book)` renders that format (accepted open items
  `- [ ]`, done `- [x] … Done DATE.`, proposals under `## Proposed` as `- [?]`),
  with a "generated by Atelier" header.
- Export: `json_canvas(book, board)` (JSON Canvas 1.0: a group node per board
  and column, a text node per todo, `file` nodes for todos with a note).
- Summaries for digests: `group_summary(book, group)`, `canvas_summary(book)`.

### 3.2 Service (`canvas-agents/src/service/todos.rs`)

- A hub like the PR and review hubs, per routed canvas id:
  `<home>/canvas-agents/<canvas id>/todos/book.json` (atomic writes) and
  `log.jsonl` (one line per applied or refused op: time, actor, op, result;
  moved to `log.jsonl.old` beyond 5 MB).
- Window actions: `ClientMsg::Bus {"action":"todos","op":<TodoOp>}` are
  applied immediately (not on the 1 s bus pass) as the user, then the snapshot
  is broadcast. A refusal reaches that window only, as
  `ServiceMsg::Error { id: None, message: "Todo change refused: …" }`
  followed by the book as it is (its optimistic change is undone).
- CLI requests: the `prs_cli::ask` request/answer pattern
  (`canvas.todo` events through `bus/inbox`, answers in `bus/answers`), the
  caller identified by `ATELIER_SESSION` (no session = you). `Answer`
  gains an optional `data` (JSON) for `--format json`.
- Clients get the book as `bus::Snapshot::todos: Option<TodoBook>`.
- Applied ops emit bus events `todo.created`, `todo.proposed`, `todo.accepted`,
  `todo.status` (`from`, `to`), `todo.claimed`, `todo.submitted`, `todo.done`
  in the group of the board's first linked canvas group, never back to the
  actor's own session.
- `pr.merged` for a pull request linked to a todo that is In progress or In
  review moves it to Done (as Bot "pull request merged"); only events from
  the service itself (the PR monitor, session history) count, never inbox files.
- The mirror file, when set, is rewritten after every change.
- Group catalogs: `bus::ensure_catalog` (the `EnsureCatalog` request and each
  group passive run) gives an existing `events.yaml` without `todo.*` events
  the seven default ones (`bus::upgrade_catalog`: lines inserted after its
  last event, only when the result reads as the same catalog plus them).
- Digests: `group-context.md` gains `# Todos` (boards linked to the group:
  counts, up to 20 open todos), `canvas-context.md` gains a per-board summary.

### 3.3 CLI (`atelier todo …`, `todos_cli.rs`)

```
todo prime                                   boards linked to my group, statuses, types, rules, commands
todo boards [--tree] [--format json]
todo list [--board B] [--status S] [--type T] [--mine] [--all] [--format json]
todo show ID [--format json]
todo ready [--board B] [--format json]
todo add TITLE --type T [--board B] [--priority P] [--status backlog|todo] [--body FILE|-] [--parent ID] [--label L]… [--check TEXT]…
todo propose TITLE --type T --why TEXT [--board B] [--discovered-from ID] [--priority P] [--body FILE|-]
todo update ID [--title T] [--type T] [--priority P] [--blocked WHY|none] [--blocker yes|no] [--body FILE|-] [--label L|-L]…
todo move ID --status S [--board B]
todo claim ID [--session NAME|ID] [--force] · todo release ID
todo note ID TEXT · todo check ID N [--undo] · todo check ID --add TEXT
todo link ID (--pr URL | --note PATH | --session ID | --url URL | --file PATH)
todo relate ID (--blocked-by ID | --related ID | --duplicate-of ID)
todo submit ID [--pr URL] [--summary FILE|-]
todo accept ID [--status S] · todo decline ID [--reason TEXT] · todo done ID · todo cancel ID [--reason TEXT] · todo reopen ID · todo delete ID
todo board add NAME [--prefix P] [--parent B] · todo board link B [--group NAME|ID]
todo import FILE [--board B] [--prefix P] · todo mirror [--out FILE | --set FILE|off] · todo export --canvas FILE [--board B]
```

Reads (`prime`, `boards`, `list`, `show`, `ready`, `mirror` without `--set`,
`export`) read the stored book directly (the routed canvas from
`bus/routing.json`); writes go through the service (waiting up to 10 s for
its answer). `--session` (people only) assigns the todo to a session of the
canvas; `--label -L` removes a label; `todo mirror --set` asks the service to
keep FILE (an absolute `.md` path on its machine) up to date, `--set off`
stops it; `--out FILE` writes one snapshot. JSON output: `{"schemaVersion":1,"kind":"todo-list","todos":[…]}`
and similar (`todo`, `todo-ready`, `todo-boards`, `todo-change`). The default
board is the first board linked to the caller's group; outside a group (you,
canvas-wide agents, automations) the canvas's only board when it has exactly
one; otherwise writes ask for `--board`.

Skill `canvas-todos` (`todos_cli::SKILL`): run `todo prime` first; use
`ready` → `claim` → `note`/`check` → `submit --pr`; `add` when the user asked,
`propose --why` when it is your own idea, `--discovered-from` for work found
while doing another todo; never edit mirror files; Done is the user's (or a
merged PR's).

### 3.4 Tasks space UI (canvas-core `tasks/`)

- **Sidebar**: NEEDS YOU (proposed, in review, stale claims: claimed by a
  session that is gone or stopped), READY (boards linked to the last viewed
  group, else all), CLAIMED BY SESSIONS, BOARDS (tree, `+ New board`); `/`
  filters; a type filter (its button, or `t` for the next type).
- **Todo canvas** (main): board frames at their place (world cells), drag the
  title to move, `◢` to resize, drag the background to pan, wheel / Shift+wheel
  / Ctrl+arrows pan (arrows only while nothing is selected),
  `+`/`-` or Alt+wheel / Ctrl+wheel zoom, `F` shows every board: 100 % full cards, 50 % id and title, 25 % a
  summary per board (counts per column, progress, blocked). Columns: Proposed
  (when any, or the policy is Proposed), Backlog, Todo, In progress (`n/wip`
  in gold when over), In review, Done (collapsed to a count); Canceled only as
  a count in the header. Sub-boards are swimlanes. Cards: type glyph and
  coloured edge, id, `‼`, priority marks (`!!!` urgent, `!!` high, `!` medium),
  title (two lines), badges (checklist `☑ 2/5`, `⚑ blocked`, `blocks N`, `◇` AI-created,
  `⟳ session` while that session works, PR state). Click selects (and shows
  the details), arrows move the selection, Enter opens details, Space peeks,
  drag a card to change status / board / order, `N` new todo in the selected
  column, `A` / `D` accept / decline a proposal, `E` edits the body, `R`
  renames, `Del` cancels (asks).
  Board title chips: `◫ group`, `✎ notes`; board menu: new todo, rename,
  prefix, WIP, agent policy (agents' todos as proposals; agents may close),
  link canvas groups, link notes folder, new swimlane, mirror, export, delete
  (empty boards only).
- **Details** (right pane, `RightDetail::Todo`): fields with pickers (type,
  priority, status), blocked reason, blocker, checklist (toggle, add), body
  (edited with the editor), links (open note, view session, open PR),
  relations, log, history. *Accept* / *Decline* for proposals.
  *Start a session on this…* opens the new-session dialog prefilled (group:
  the board's linked group; name: `<ID> <title>`; first prompt: "Work on
  DEC-031 (title). Run `atelier todo show DEC-031`, then
  `atelier todo claim DEC-031`."): the
  dialog is the explicit start. *Open spec note* opens the linked note, else
  creates `<root>/Todos/<ID> - <title>.md` with the body, links it, and opens
  it. *Claim for…* (you or a session), *Release*, *Done*, *Cancel*, *Reopen*.
- Window edits apply locally at once (optimistic) and are sent as ops.
- Demo mode shows a small demo book.

## 4. GUI web pane (canvas-gui `webview.rs`, `webpane.rs`, core `view.rs`)

- `View::html` becomes a pane (`HtmlPane { rect, id, source, origin }`;
  `origin`: `HtmlOrigin::Report` or `Note`, so a clicked web link asks in
  words that say where it comes from) with a source:
  `HtmlSource::Document(Arc<String>)` (reports, rendered Markdown, pictures) or
  `HtmlSource::File { path, root, scripts }` (an HTML note; `root`: the
  explorer root it is in).
- Two WebView2 views: the plain one never runs scripts; an HTML note allowed
  to run its scripts is shown in a second one, created when first needed in
  its own in-private profile (`canvas-scripts`), whose page becomes
  `about:blank` whenever it is hidden.
- Files are served from `https://notes.canvas.invalid/<path relative to root>`
  through `WebResourceRequested` with bytes from `Request::ReadBytes` (deferred
  and completed on the UI thread; read on 4 worker threads; at most 256
  waiting per window, a replaced page's answered at once), so relative
  images and CSS work locally and remotely. Only `GET`s of files in the
  note's own folder and the folders below it are served (a document only
  when it is the note); links to other files under `root` open them in the
  app. The main document
  is sanitized (`report_html::notes::page`) unless scripts are allowed;
  every response carries a CSP header (and `nosniff`, `Cache-Control:
  no-store`, `Referrer-Policy: no-referrer`, `X-DNS-Prefetch-Control: off`;
  the scripts view also `Access-Control-Allow-Origin: *`):
  - default: `default-src 'none'; img-src https://notes.canvas.invalid data:; style-src 'unsafe-inline' https://notes.canvas.invalid; font-src data: https://notes.canvas.invalid; media-src https://notes.canvas.invalid data:; script-src 'none'; connect-src 'none'; frame-src 'none'; form-action 'none'; base-uri 'none'`
  - scripts allowed for this file: `sandbox allow-scripts; default-src 'none'; script-src 'unsafe-inline' https://notes.canvas.invalid; style-src 'unsafe-inline' https://notes.canvas.invalid; img-src https://notes.canvas.invalid data: blob:; font-src data: https://notes.canvas.invalid; media-src https://notes.canvas.invalid data: blob:; connect-src 'none'; object-src 'none'; frame-src 'none'; child-src 'none'; worker-src 'none'; form-action 'none'; base-uri 'none'` (never `allow-same-origin`, no other hosts, no frames or workers).
- Every other request is answered 403; navigations other than the page's own
  load are cancelled; user links ask before the browser; `#note:` / `#todo:`
  links (and links to other files under `root`) become app navigations (in
  the scripts view only from the user's clicks: while it has the keyboard,
  at most once a second, one per click into it; the keyboard then comes
  back to the canvas). The
  browser process gets a dead proxy (`--proxy-server=127.0.0.1:9`, loopback
  too: `--proxy-bypass-list=<-loopback>`), no DNS
  prefetching and WebRTC only through the proxy, besides the reports' flags;
  SmartScreen lookups and external drops are off.
- The pane shows only while no dialog or menu overlaps it or the top one
  holds it whole (`webpane::pane_clear`; the Notebook publishes no pane while
  a dialog is open) and keeps the global keys: `Esc`, `F6`, `Ctrl+]`,
  `Ctrl+1/2/3`, `Ctrl+P`, `Alt+←/→`, `Alt+,` / `Alt+.`, `Ctrl+E`, `Ctrl+W`,
  `Ctrl+Tab`, `Ctrl+Shift+Tab`, `Ctrl+N`, `Ctrl+Shift+N`, `Ctrl+S`, `Ctrl+F`
  and `Alt+N` are forwarded to the core. Esc closes a report; from a note it
  gives the keyboard back to the canvas.
- *Print / PDF* uses WebView2's print UI (`ShowPrintUI`) once the page has
  loaded; without it (an older runtime) the page is saved as
  `<Downloads>\<title>.pdf` (`-2`, `-3`: never over a file, each name
  claimed by creating it).

## 5. Editor (canvas-core `editor.rs`)

A standalone multi-line editor drawn into cells (used by the notebook and the
todo body): lines + cursor + selection, soft wrap to the width, scrolling,
undo/redo (grouped typing; at most 2,000 steps), clipboard through the caller (copy/cut text out,
paste text in), word moves, Home/End, PgUp/PgDn, Ctrl+Home/End, Shift
selection, Ctrl+A, Tab/Shift+Tab indent (two spaces), Enter continues list items and
quotes, find (Ctrl+F; F3 / Shift+F3 or Enter / Shift+Enter next/previous, highlighted), Markdown styling in Source
mode (headings, emphasis, code, links, list markers, quotes, fences, front
matter dimmed), and **Live** mode: lines without the cursor are shown
formatted with their syntax hidden (headings without `#`, emphasis without
markers, links as their text), the cursor's line raw; clicks map back to the
raw text. The caret position is reported for the front end's caret and IME.

## 6. Tests and verification

- Unit tests for every new module (model logic, provenance, frontmatter,
  ops and policy, import/mirror/export, editor behaviour, CLI parsing,
  service handling with temporary homes, backend requests with temporary
  folders), backend contract tests for new requests, view snapshots
  (`crates/canvas-core/tests/view_snapshots.rs`) for the new screens.
- Never touch the owner's real `~/.atelier`; tests use temporary homes.
- `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo test --workspace` pass. GUI self-tests, parity rows and goldens are
  run sequentially at the end (they open windows).

## 7. The iPhone app, and what is not covered

The iPhone app (2026-11-02; ios/README.md, docs/IOS-STATUS.md §1.1) has
**Notebook** and **Tasks** places next to Sessions and Canvas in a bottom
tab bar. It is not parity with the desktop: it shows and edits notes and
todos.

- **Notebook:** the notebook root's folder with the other roots, folders in
  a navigation stack, entries with their provenance (notes for agents
  dimmed), the All / For you / For agents filter, the canvas's own files on
  request, search, pull to refresh. A note opens as its sanitized page in the
  locked-down `WKWebView` the reports use (`#note:` links open notes in the
  app, `#todo:` links todos, web links ask before Safari, pictures inlined
  by the core). The phone never runs an HTML note's scripts (the desktop
  can allow them per file). Mark reviewed, copy, share, rename, delete to
  the trash. *Edit* opens a plain-text source editor (Markdown, HTML, text
  files): saves on the revision it opened; a note changed elsewhere asks
  *Reload* (after confirming) or *Keep mine* (after a second confirmation),
  as in §5; unsaved changes ask before closing. New notes are for you, by
  you (`notebook::your_new_note`).
- **Tasks:** Needs you first (accept into a status, decline with a reason,
  release a gone session's claim), then one board as a list by status
  (swimlanes as captions inside each status; Done folded). A todo's details:
  fields, the proposal, the claim, the checklist (tick, add, reword,
  remove), the body as a page, links, relations, the log; Move, Done,
  Cancel, Reopen, Release where the desktop offers them; Edit (title, type,
  priority, labels, blocker, blocked reason, body); New todo.
- Not covered: the todo canvas's board layout, board editing, claims for a
  session, starting a session on a todo, Live preview, backlinks and the
  outline, quick capture (Alt+N), the notebook and mounts settings, scripts
  in HTML notes.
- Switching the live TODO bot (outside this repository) to the store; the
  commands it needs exist (`todo import`, `ready`, `claim`, `submit`, `mirror`).
