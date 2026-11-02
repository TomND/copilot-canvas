---
name: canvas-ui-testing
description: How to test Copilot Canvas UI changes - ViewModel snapshots of canvas-core states, the GUI's test hooks (and driving a running GUI by hand), GUI parity rows that are robust (waits, exact UI texts, shared helpers, cleanup), golden screenshots (updating and reviewing them) and the hardening checks. Use when you add or change anything drawn in the TUI or the GUI, when a snapshot, self-test step, parity row or golden screenshot fails, when you write a new parity row or self-test step, or when you need to see what the GUI shows during a test.
---

# Testing the UI

Depth and file maps: `docs/DEVELOPMENT.md` section 3; the hook commands:
`docs/GUI.md` section 3; the matrix: `docs/PARITY.md`. Run GUI suites one at
a time (they open windows on the owner's desktop); procedure in the skill
`canvas-verify`.

## View snapshots (every screen state, both clients)

- File: `crates/canvas-core/tests/view_snapshots.rs` (helpers `app`, `node`,
  `summary`, `term`, `check`, `check_sized`); facility `tests/snapshot/mod.rs`;
  texts in `crates/canvas-core/tests/snapshots/<name>.snap`.
- A test builds an `App` in-process (`app(doc, demo)`: detached connection,
  `Offline` backend, path `C:\work\canvas`, tick 0, motion on), sets the state
  (selection, a dialog, a view), then `check("<name>", &mut app, BOTH)`. It
  records the TUI screen, the GPU view model (`View::dump`) and any dialog
  overlay; `Sections { cells: true, painted: true }` adds the GPU chrome with
  the canvas model painted as text.
- Nothing from the clock or the machine: fixed ids, paths and times.
- Run one: `cargo test -p canvas-core --test view_snapshots <test name>`.
- A mismatch writes `<name>.snap.new` and prints a line diff. Accept with
  `$env:UPDATE_SNAPSHOTS = '1'; cargo test -p canvas-core --test view_snapshots; Remove-Item Env:UPDATE_SNAPSHOTS`,
  then read every `.snap` diff (`git diff -- crates/canvas-core/tests/snapshots`)
  and explain it in the commit. Unexplained changes are bugs.

## GUI test hooks

`copilot-canvas-gui.exe --test-hooks` prints a pipe name
(`\\.\pipe\copilot-canvas-gui-test-<pid>`, owner-only) and takes one JSON
command per line, answering one JSON line (`canvas-gui/src/hooks.rs`,
dispatched in `shell.rs`). The ones you need most:

| Command | For |
|---|---|
| `{"cmd":"screen"}` | `lines`: the screen as text (chrome, canvas as TUI characters, terminals, dialogs) |
| `{"cmd":"tree"}` | `tree`: click targets with text, canvas boxes (status, border, deck, badge, rect), groups and chips, tiles, focus, modals, notice, `access` |
| `{"cmd":"key","key":"Enter"}`, `{"cmd":"key","char":"a","mods":["ctrl"]}`, `{"cmd":"text",…}`, `{"cmd":"paste",…}` | keyboard |
| `{"cmd":"mouse","kind":"down","button":"left","x":40.5,"y":7.5,"mods":[]}`, `mice`, `wheel` | mouse at fractional cells |
| `{"cmd":"png","path":"…"}` | an offscreen capture (never contains the WebView2 view; use the `webview` hook for HTML) |
| `{"cmd":"freeze","tick":0}` | hold animations still |
| `{"cmd":"quit"}` | save and exit (detach) |

Driving a GUI by hand (a sandboxed instance, a PowerShell
`NamedPipeClientStream` client) is shown in `docs/DEVELOPMENT.md`, *Driving
a GUI by hand*; stop that instance's service afterwards with
`copilot-canvas service stop --instance <name> --home <dir>`.

## Parity rows (`src/parity.rs`, `src/parity_*.rs`)

- Register with `check(rows, "<row>", "<feature>", "<test_name>", || body)`
  (or `check_kind` with `Kind::Only("<why>")`); the body returns
  `Ok(detail)` or an error that says what was awaited.
- Rows that use the shared GUI go in `main_rows` or a module called from it
  (`parity_reports`, `parity_agents`, `parity_prs`, `parity_spaces`,
  `parity_skills`); rows on canvases of their own or with other clients go in
  `late_rows` (`parity_visual`, `parity_layout`, `parity_subagents`,
  `parity_pulse`).
- **Wait, never sleep instead:** `gui.wait(what, secs, |screen| …)`,
  `gui.wait_tree(what, secs, |tree| …)`, `wait_doc(&sb.canvas, what, |doc| …)`.
- **Exact UI texts:** find things by the strings canvas-core draws
  (`gui.find("Skill folders…")`) and boxes by name (`gui.box_center`). When
  you rename a UI text, search `src/parity*.rs`, `src/selftest*.rs`,
  `src/golden.rs` and `src/hardening.rs` for it.
- **Shared helpers:** `Gui::canvas_focus`, `Gui::open`, `group_menu` (a
  right-click on a free cell of the parity group's title), `model_dialog`,
  `pick_model`, `create_from_dialog`, `stop_box`, `inspect_in_deck`; `f`
  shows every box.
- **Nothing started by viewing:** compare the count of `launched ` lines in
  `sb.instance.log_path()` before and after.
- **Cleanup:** later rows run in the same GUI. Restore what the row changed
  even when it failed (compute the result, clean up, return the result
  first: `parity_skills.rs:skills`, `parity_prs.rs:after`), close dialogs and
  end with `gui.canvas_focus()`.
- **Subset while working:** `$env:CANVAS_PARITY_ROWS = '52'` (plus the rows
  it builds on), then the full suite.
- **Docs:** the row in `docs/PARITY.md` and its *Last run* section.

## Self-test steps (`src/selftest.rs`)

Write steps against the `Ui` trait only (`send`, `paste`, `text`, `wait`,
`click`, `drag`, `find`, `locate`) so they run in the TUI's hidden
pseudoconsole, in the GUI and over `--remote loopback`; announce each with
`step("…")`; update the step count (45) where the docs give it.

## Golden screenshots (`src/golden.rs`, `tests/golden/gui/*.png`)

- `selftest --client gui --golden` compares scripted, clock-free states with
  the stored PNGs (channel delta 24, at most 0.5 % of pixels, 8 × 8 block
  luminance delta 16; masked regions hatched).
- A failure leaves `<name>.actual.png` and `<name>.diff.png` beside the
  golden: open them.
- `--update` rewrites them all. Look at every changed PNG against its old
  version (`git diff --stat -- tests/golden`), keep only intended changes
  (`git checkout -- tests/golden/gui/<name>.png` for the rest), name them in
  the commit. Goldens belong to the reference machine (RTX 3070, 125 % DPI);
  elsewhere a mismatch may be the machine, not a regression.

## Hardening (`src/hardening.rs`)

`selftest --client gui --hardening`: nine GUI checks (toolbar counts at 170
columns, overlapping boxes, device loss, DPI change, window placement, the UI
Automation tree, crash logs, the software renderer, damage frames). Run it
after changes to the GUI shell, rendering, accessibility or window handling.
