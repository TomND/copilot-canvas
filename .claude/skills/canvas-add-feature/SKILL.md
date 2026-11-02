---
name: canvas-add-feature
description: Checklist for a Copilot Canvas change a user can see (a new view, panel, dialog, menu item, key, chip, canvas visual, command or skill text) so it works in the TUI and the GUI, locally and in remote mode, stays compatible with older services and clients, starts nothing by viewing, and ships with tests at every layer and updated docs. Use when you plan, build or review a feature or a visible behaviour change in this repository, before you call it done, and before you commit it.
---

# Adding a feature a user can see

Where the code goes (recipes with file names) is `docs/DEVELOPMENT.md`
section 2; the rules are in `AGENTS.md`. Walk this list before you start and
again before you commit.

## Design

- [ ] **The README is the specification.** Read the feature's row in the
  README *Features* reference (and *Controls*, *Commands*) first; the change
  must leave it true.
- [ ] **Both clients.** UI state, layout and key handling live in
  `canvas-core` (cells), so the TUI and the GUI get them together. The GUI
  adds pixel polish only (`canvas-gui`). HTML is GUI-only with a formatted
  text fallback in the TUI. Anything GUI-only is recorded in
  `docs/PARITY.md` (*New in the GUI*) with the reason.
- [ ] **Remote mode.** A client never touches the service machine's files or
  processes: every read or change goes through a backend request
  (`canvas_model::rpc`, handled in `canvas-agents/src/backend.rs`) or the
  service (bus actions, document edits). Paths shown and stored are the
  service machine's. Folders open in the canvas's viewer or VS Code through
  the tunnel when remote (`folders.rs`), never this machine's Explorer.
- [ ] **Viewing never starts anything.** A click, Tab, the arrows, a badge, a
  link, a chip or a sidebar row only views. Only a double-click, Enter, an
  explicit Start / Resume button or a menu item starts or resumes a session
  (WI-007). Sessions open elsewhere never get a Start.
- [ ] **One writer.** The service owns `canvas.json` and every store
  (reports, PRs, reviews, todos). Clients send document edits or bus
  actions and apply what comes back; the service never originates document
  patches. Service-owned state reaches clients in the bus snapshot.
- [ ] **Compatibility.** Older services and clients stay in use. New JSON
  fields default and skip when unset; wire enums only grow at the end;
  postcard messages never gain fields; no new `NodeKind`; a new shared
  document field needs a current service. Follow the skill
  `canvas-wire-compat` for anything that crosses the wire or the disk.
- [ ] **Agents and skills.** If agents should use the feature, the skill text
  in `canvas-agents` (`cli.rs`, `todos_cli.rs`, `notes_cli.rs`) changes with
  its contract test, and the plugin version moves; agents get it once the
  service restarts on the new build.
- [ ] **Glyphs and text.** Every glyph one cell wide in the Windows console
  (`≡`, not `☰`); no emojis in code or UI text; colour is never the only
  signal (the palette is colour-blind safe: add an icon or border too).
- [ ] **Demo mode.** The demo has no passive sessions, bus or real agents;
  decide what the feature does there (usually disabled, as the group menu's
  passive items are).

## Tests (every layer that applies)

- [ ] Unit tests next to the code (`canvas-core` state and actions, model
  rules, service handlers).
- [ ] View snapshots (`crates/canvas-core/tests/view_snapshots.rs`) for every
  new or changed screen state; read every `.snap` diff before accepting.
- [ ] A backend contract test (`tests/backend_contract.rs`) for a new request:
  local and remote answers are the same.
- [ ] A self-test step (`src/selftest.rs`, against the `Ui` trait so it runs
  in the TUI and the GUI) and/or a GUI parity row (`src/parity*.rs`) for the
  behaviour end to end; a "nothing started" check where viewing is involved.
- [ ] Golden screenshots re-blessed only after looking at them, when the look
  of a captured state changed (skill `canvas-ui-testing`).
- [ ] The whole verification (skill `canvas-verify`).

## Docs (in the same commit)

- [ ] README: the *Features* row, *Controls* for keys, *Commands* for CLI.
- [ ] The topic doc: `docs/BACKEND.md` (wire and stores), `docs/GUI.md`,
  `docs/AGENT-PROVIDERS.md`, `docs/NOTEBOOK-TASKS.md`, `docs/REMOTE.md`.
- [ ] `docs/PARITY.md`: the row (matrix, *New in the GUI* or *Canvas
  visuals*) and the *Last run* section with the numbers of your run; step and
  row counts where the docs give them.
- [ ] The in-app help (`ui_modals.rs` help rows) for new keys.

## Commit

One subject line that says what changed and why, often long, in the style of
`git log`; a backlog item starts with its id (`WI-023 …`). Include what you
verified (suites, counts, times). Do not push unless asked; never force-push.
Fetch before merging: the TODO bot pushes to `main` every two hours.
