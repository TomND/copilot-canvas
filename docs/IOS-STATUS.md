# iPhone app: status, next steps, leftover work

Status on 2026-10-30, `main` at b29b566; the core's todo boards and
notebook (§1.1) were added on 2026-10-31, the app's Notebook and Tasks
places (viewing and editing) on 2026-11-02. This file describes where the
app is today. The design and the phase history are in
[IOS-PLAN.md](IOS-PLAN.md); how to build, run and use the app is in
[../ios/README.md](../ios/README.md).

## 1. Where it is

The iPhone app is merged into `main`. It started on the `ios-client` branch
(PR #1, merged as bded559); the reports inbox came after the merge (6deb60c).
It is a remote client of the Atelier service. It connects to the
service over the same private Dev Tunnel as the desktop's remote mode, using
WebSocket web forwarding, a 24 h tunnel access token and a one-time pairing
code. Most of the logic is in a shared Rust core (`crates/canvas-mobile`,
built with UniFFI). The interface is SwiftUI (`ios/`).

| Area | State |
|---|---|
| Pairing | QR code, `atelier://` link (or `ccanvas://`, from before the rename), or the code typed by hand. Pairing the same address again replaces it and keeps the machine's name and colour. Credentials are kept in the Keychain. Face ID / passcode lock after a minute away |
| Several machines | Every paired machine stays connected while the app is open. A switcher shows each machine's state, what needs you and its new reports. Each machine has a colour, and an attention strip shows sessions that need you on the other machines |
| Sessions | Grouped by what needs you, then by canvas group. Subagents, subsessions, loops, passive sessions and queued mail are shown. Actions: stop, remove, rename, mark reviewed |
| Canvas | Groups, boxes, status borders and edges like the desktop. Pan, pinch to zoom, fit; tap a box to open it |
| Terminal | Renders the service's terminal cells with Core Text. Key bar, hardware keys, prompt composer, quick keys. Selection and copy, links, wheel scrolling, pinch to resize text. *Fit to the phone* (the terminal follows the device that typed last, like tmux) or *Mirror* the desktop's size; jump back to live output |
| Starting sessions | Choose the agent, browse folders on the service's machine, start in a group, give a first prompt. Subsessions and forks; passive session actions (run, pause, discard mail) |
| Files and pictures | Read-only viewer for file paths tapped in a terminal. Pictures go to the service machine's clipboard and are pasted into the agent (Ctrl+V) |
| Reports | Inbox behind the tray button, one row per thread, read state shared with every window. HTML, Markdown and messages each open as one sanitized page in a locked-down `WKWebView`. Links ask before Safari opens them. A toast when a new report arrives while the app is open |
| Notifications | Local notifications while the app is open or just backgrounded. APNs pushes from the service while the app is away: a session needs you or finished, a new report arrived, and silent renewal of the tunnel token. An expired token shows *Pair again* |
| Service side | WebSocket link, terminal sizes per client (tmux "latest"), tunnel token rotation, `remote pair --qr`, `remote push-key`, `remote push-test`. Paired clients cannot shut the service down. A push for each new report (at most 3 at once) |
| Mac stand-in | `canvas-mock` with `ios/cloud-mac.sh` stands in for the Cloud PC behind a real private tunnel, with real agent CLIs and a demo reports inbox. `ios/cloud-mac.sh report FILE` publishes a report |
| Places | A bottom tab bar: Sessions, Canvas, Notebook, Tasks (each its own navigation stack; the machine's colour, the connection banner and the attention strip above each). + makes what fits: a session, a note or folder, a todo. Badges: sessions that need you, todos that wait for you |
| Notebook (2026-11-02) | Folders with each note's provenance (notes for agents dimmed), the All / For you / For agents filter, the canvas's own files on request, search; a note as its sanitized page (`#note:` / `#todo:` links in the app, web links ask, scripts never run); Mark reviewed, copy, share, rename, delete to the trash; a source editor (Markdown key row, undo) that saves on the note's revision and asks on a conflict; new notes and folders (§1.1) |
| Tasks (2026-11-02) | Needs you (accept, decline with a reason, release), one board as a list by status with swimlanes as captions; a todo's details (checklist to tick, add, reword, remove; body as a page; links, relations, log), Move, Done, Cancel, Reopen, Release, Edit, New todo; refusals from the service as a toast (§1.1) |

### 1.1 Todos and notebook

The desktop's Tasks and Notebook spaces (docs/NOTEBOOK-TASKS.md) are in
`canvas-mobile` as data and changes, and the app shows them as the Notebook
and Tasks places (docs/NOTEBOOK-TASKS.md §7: not parity, viewing and editing).

- **Layout decisions.** A bottom tab bar (Sessions, Canvas, Notebook, Tasks)
  replaces the 104 pt Sessions | Canvas segmented control: four places do not
  fit a segmented control next to the machine menu, the reports tray and +
  on an iPhone 14 Pro or 17 Pro in portrait, and a tab bar is the native
  pattern (each place keeps its own navigation stack; a terminal hides the
  bar). The top bar keeps the machine menu, the title, the tray and + (whose
  meaning follows the place). A board is a vertical list by status (Needs
  you above it, proposals only there, Done folded, swimlanes as captions
  inside each status): horizontally paging columns showed one column at a
  time and fought the rows' swipe actions.
- **Todos** (`todos.rs`, `Client`): `todos()` gives a `TodosOverview` (the
  book the service sends in its bus snapshot, as records: boards with their
  columns, swimlanes and cells, every todo as a `TodoItem`, what waits for
  you as `TodoNeedItem`s: proposals, todos in review, claims of sessions that
  are gone or stopped; each group's counts); `todo(id)` a `TodoDetail` (body,
  checklist, links, relations, log, the blocked reason as written, and
  `actions`: the desktop's buttons for it as you). Changes:
  `accept_todo(id, status)`, `decline_todo(id, reason)`, `move_todo(id,
  status)`, `done_todo(id)`, `check_todo(id, index, done)`,
  `add_todo(TodoDraft)` (with `blocker`), `update_todo(id, TodoEdit)` (title,
  type, priority, body, labels, blocker, blocked reason: only the fields
  given), `reopen_todo`, `cancel_todo(id, reason)`, `release_todo`,
  `add_check`, `edit_check`, `remove_check` (the last two are new
  `TodoOp::EditCheck` / `RemoveCheck` in canvas-model; an older service says
  it does not understand them). Each is applied to the phone's copy at once
  (as you, under the same rules as the desktop) and sent as the window's bus
  action (`{"action": "todos", "op": …}`), answers a `TodoChange` (the id and
  what happened, with warnings) and emits `CoreEvent::Overview`; the
  service's next book confirms or undoes it, and its refusal reaches the app
  as a toast. Offline, or before the first book, a change is refused.
- **Notebook** (`notes.rs`, `Client`): `notebook()` resolves the root
  (`Request::NotebookInfo`) and gives a `NotebookOverview` (root,
  canvas-wide folder, the explorer's roots); `list_notes(folder, filter,
  show_internal)` a `NoteListing` (folders first, each entry with its
  `NoteProvenance`: audience, author, inferred, agent, session and its box's
  name, created, reviewed); `read_note(path)` a `NotePage`: one sanitized
  page for the web view (Markdown rendered with `[[Note]]` and todo ids as
  `#note:` / `#todo:` links, HTML sanitized like reports, other text as a
  block, pictures from its root inlined as `data:` URLs: at most 16
  pictures, 4 MiB each, 12 MiB in all; a picture note at most 8 MiB), so the
  web view loads nothing; `note_revisions(paths)` (an open note reloads when
  it changes), `search_notes(query, limit)`, `resolve_note(name)` (a
  `[[Name]]` link's note), `mark_note_reviewed(path)`, and the free function
  `note_link_target(href)`. Changes, with the explorer's roots so the
  service enforces containment: `note_source(path)` (text, revision, whether
  the phone may edit it and why not: Markdown, HTML and text files inside a
  root), `save_note(path, text, base)` (`NoteWrite` on that revision; an
  agent's note gets `author: mixed`; a note changed meanwhile is
  `CoreError::Conflict` with its text and revision now, nothing written),
  `create_note(folder, name, text)` (`.md` unless named otherwise, for you
  and by you, never over a file), `create_folder`, `rename_note` (keeps the
  extension), `delete_note` (`NoteTrash` into its root's `.trash`).
- **The app**: the web view's in-page `#note:` / `#todo:` links become
  `canvas-app:` links before loading (WebKit never asks about jumps within a
  page), which the view cancels and hands to the app. Alerts with a text
  field keep their keyboard (the app-wide tap-to-dismiss skips them, or
  their buttons lost the tap).
- **The mock** (`canvas-mock`): a demo notebook in `<state>/notebook`
  (`Welcome.md`, an Inbox idea, a Payments group with `ai-docs/GOALS.md`, an
  investigation log, a release plan for you, an HTML dashboard whose script
  and network picture must never run or load, a picture, the group's
  instructions file), seeded at every start (missing files only) and
  answered like the service's notebook requests (writes only inside it; the
  canvas's notebook setting is not followed). The demo todo book
  (`todos::demo_book`, "Canvas App" linked to the Payments group, claims by
  the mock's sessions) is in every bus snapshot, in memory; window todo
  actions are applied as you and broadcast, a refusal comes back as an error
  with the book.

## 2. How it is verified

| Check | Covers |
|---|---|
| Rust unit tests (`canvas-mobile`, `canvas-common`, `canvas-model`, `canvas-push`) | State machine, statuses, launches, forks, passive sessions, the reports inbox and its pages, the todo boards (the book from the bus snapshot, changes applied here and sent as the window's action, field edits, the desktop's actions per todo, checklist edits, refusals, offline) and the notebook (pages, what may be edited, `author: mixed`, new notes, renames, the trash), push payloads, JWT signing, sealed tokens |
| `crates/canvas-mobile/tests/end_to_end.rs` | The core against the mock over a real WebSocket: pairing, a shell typed into, reconnecting, the reports inbox across two phones, the todo boards and the notebook (boards from the snapshot, a change reaching the other phone, the notebook's root, folders with provenance, notes as safe pages, *Mark reviewed* on the note's revision), and edits across two phones: a note saved on its revision, a conflict when it changed meanwhile and *Keep mine*, an agent's note becoming mixed, new notes and folders, renames, the trash; todo fields, checklist add/reword/remove/tick, cancel and reopen, release |
| XCTest (12 tests) | Core bridge, terminal helpers, the report view's lock-down against a local network listener, and a note page in it (app links to the app, web links asked about, nothing loaded) |
| XCUITest (9 tests, against two mocks) | Session list, canvas, typing into a shell, new session form and keyboard, two machines, subsessions, passive pause/resume, opening an HTML report; the Notebook (open a note, follow a `#note:` link, create a note, write and save it in the editor with the key row, see it, delete it) and Tasks (decline a proposal and reopen it, tick and untick a checklist item, create a todo, edit it, cancel it) |
| CI (`.github/workflows/ci.yml`) | All of the above on macOS. Windows: clippy, the service's tests, the GUI self-tests and the parity suite |
| By hand, on Thomas's iPhone 14 Pro | Pairing and daily use through real `*.devtunnels.ms` tunnels against the two Mac stand-ins |

## 3. Not verified yet (needs Thomas)

1. **The real Windows Cloud PC with the phone.** Update the Cloud PC from
   `main` (the protocol indices changed, so the phone and the service must
   both be current). Then:
   - run `atelier remote pair --qr` and scan it;
   - check terminal sizes while the desk and the phone take turns typing;
   - check Copilot, OpenCode and Claude Code at phone width;
   - start a new session and a fork from the phone;
   - check passive session actions;
   - check reports from real agents.
2. **APNs delivery.** Create the `.p8` key (developer.apple.com › Keys ›
   Apple Push Notifications service). Then run `atelier remote push-key
   AuthKey_ID.p8 --team 8Z37ZVMJ4V` and `remote push-test`. Check four
   notifications with the app closed: a session needs you, a session
   finished, a new report, and a silent token renewal. For the Mac
   stand-ins: `APNS_KEY=… APNS_TEAM=8Z37ZVMJ4V ios/cloud-mac.sh pair`.
3. **A day away.** Check that the token is renewed silently while the app is
   closed (this needs the key). Also check the expired-token banner and
   *Pair again* on the device.
4. **The reports inbox on the device.** The build is installed but has not
   been opened yet.

## 4. Next steps (in order)

1. **Validation on the Cloud PC** (§3.1). Fix what it finds.
2. **APNs key and push validation** (§3.2–3.3).
3. **TestFlight.** Today the app is installed from Xcode with a
   development profile, so pushes use the sandbox. To move to TestFlight:
   - create an App Store Connect record and archive a build;
   - switch the entitlement to `aps-environment` `production` for
     distribution builds. The app already reads the environment from its
     profile and tells the service;
   - add an app icon. The project names `AppIcon` but has no asset catalog
     yet; the desktop icon in `assets/icon/` can be reused.
4. **Tunnel access without re-pairing.** The 24 h tunnel token is renewed
   while the phone is connected or by a silent push. If the phone is away
   long enough, you have to scan a new QR code. Signing in with GitHub on the
   phone, using the tunnel owner's identity, would remove this. Its design
   still needs to be written.
5. **iPad.** A split view, with the session list or canvas beside a terminal.
6. **Inline images in terminals** (Kitty graphics). The core already keeps
   the service's `Image` and `Placements` messages; the Core Text renderer
   does not draw them yet.
7. **More of the desktop's Notebook and Tasks** on the phone, if wanted:
   quick capture, backlinks, claims for a session, *Start a session on
   this…*, board settings (docs/NOTEBOOK-TASKS.md §7).

## 5. Leftover work and known gaps

| Item | Notes |
|---|---|
| Notifications for automation failures | Listed in phase 5 of the plan, but the service only pushes for attention, reports and tokens |
| Canvas editing | Moving or resizing boxes and groups and editing groups stays on the desktop (D5, light client). The phone edits only open/review, rename, new session and remove |
| Automations, MCP, settings, VS Code views | Desktop only |
| PR coordination and review follow-ups (WI-014/016) | Their results reach the phone as reports and session statuses; their panels are desktop only |
| Metal renderer | Only if Core Text becomes too slow for large screens or fast output |
| QR code in the GUI's remote sheet | Today the QR code comes from `remote pair --qr` in a terminal on the Cloud PC |
| Repeated report alerts | A report that arrived while the app was away gets a push, then a toast when the app reconnects. This is minor; the core could drop toasts for reports older than the last disconnect |
| Older services | A service without reports never sends the inbox, and the inbox screen says so. The protocol indices changed during the merge, so services built before 7ad6e71 cannot talk to this app |
| The mock is not the service | The mock ignores bus actions other than reports and todos. Pausing works through the canvas field. Its notebook is always its demo folder and its todo book lives in memory (every start begins with the demo book; no `atelier todo` requests, no events). UI tests run against the mock, so the real service's bus behaviour is covered only by the Windows tests |
| Todos and notes on the phone | Viewing and editing (§1.1), not parity: no todo canvas, board settings, claims for a session or *Start a session on this…*; no Live preview, backlinks, outline or quick capture; HTML notes never run their scripts on the phone (the desktop can allow them per file); text files are editable on the phone, read only on the desktop. The source editor is plain text (no find, no syntax colours) |
| Accessibility, localisation | English only. VoiceOver labels exist for controls and the terminal's text value; there is no full VoiceOver pass |
| Test gaps | No automated test of toasts, local notifications or APNs delivery, and no on-device automation |

## 6. Useful commands

```sh
ios/build-core.sh && (cd ios && xcodegen generate)      # Rust core + Xcode project
cargo test -p canvas-mobile -p canvas-push -p canvas-mock # core, push, mock
./target/debug/canvas-mock --listen 127.0.0.1:47600 --cwd "$PWD" &   # UI tests need both mocks
./target/debug/canvas-mock --listen 127.0.0.1:47601 --state /tmp/canvas-mock-personal --cwd "$PWD" --name personal &
(cd ios && xcodebuild -project Atelier.xcodeproj -scheme Atelier \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' test)
ios/cloud-mac.sh start | pair | status | stop | logs | report FILE   # this Mac as a stand-in Cloud PC
```
