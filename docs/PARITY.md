# GUI parity (PLAN §5 Phase 5, §8)

Every row of the parity matrix in `PLAN.md` §8 works in the GPU client and
has an automated GUI check. The checks run in one command:

```powershell
cargo build --release
target\release\copilot-canvas.exe selftest --client gui --parity
```

It takes about 270 s on the development machine (rows 40b, 42 and 43 wait out event debounces, an owner's
20 idle seconds and stand-in agent runs; RTX 3070), prints each check as it runs and ends with the table below;
`CANVAS_GUI_SCREENS=DIR` keeps the PNG captures the checks inspect.
`CANVAS_PARITY_ROWS=41,41b` (a comma-separated list of row numbers) runs only
those rows, for work on one feature; rows outside the list are not run and
not listed, and the full suite is what counts.

## How the suite works

- **Over the test hooks.** It starts `copilot-canvas-gui.exe --test-hooks
  --size 170x46 --perf` and drives it like the 45-step self-test
  (`docs/GUI.md` §3): keys, text, mouse and wheel at cell centres, the `screen`
  text, the `tree` (boxes with status, border and loop; tiles with images,
  tint and links; selection; notices), offscreen `png` captures whose pixels
  are checked, and the new `platform`, `clipboard`, `drop`, `new_window`,
  `windows` and `close_window` commands.
- **Sandboxed and safe.** Everything lives in a temporary folder: the canvas
  home, a `COPILOT_HOME` with the sample automation database, and synthetic
  Copilot histories (`workspace.yaml` + `events.jsonl`, live ones holding an
  `inuse.<pid>.lock`) for every status: working, needs you (a pending
  permission), done · review, idle, stopped, failed, open elsewhere, a
  background subagent, a recurring `session.schedule_created` loop. OpenCode
  gets a synthetic database (`XDG_DATA_HOME`: a session with a working and a
  finished task-tool child, for the inspector row); Claude Code looks in an
  empty folder (`CLAUDE_CONFIG_DIR`).
- **No agent, no model.** Terminals are PowerShell shells. The agent is a
  stand-in Copilot CLI: `copilot-canvas.exe` copied as `copilot.exe` (and as
  `opencode.exe` for the OpenCode boxes) and run with
  `COPILOT_CANVAS_FAKE_COPILOT=1` (`COPILOT_CANVAS_COPILOT` and
  `COPILOT_CANVAS_OPENCODE` point the canvas at it). It lists two MCP servers, answers `config --list`, and as a
  session prints the MCP servers it was told to turn off, a message of yours
  (for the tint), a reply, enables SGR mouse reports and echoes keys and
  clicks until Ctrl+D or Ctrl+C.
- **Clipboard on a locked workstation.** Windows refuses the clipboard while
  the workstation is locked (`OpenClipboard` fails with access denied). With
  `--test-hooks` the GUI's platform keeps a clipboard of its own that stands
  in only then: copies are recorded there and the `clipboard` hook can put
  text or an image on it. The checks report which clipboard they used; on an
  unlocked desktop they go through the Windows clipboard.

## The matrix

`GUI check` is the parity suite's check; `also` lists the 45-step GUI
self-test steps (`selftest --client gui`, also run with `--remote loopback`)
and unit tests that cover the row.

| # | Feature (§8) | GUI check | Also | ✓ |
|---|---|---|---|---|
| 1 | Canvas: drag boxes (subagents follow), pan, zoom, Show all nodes, center, double-click creates, right-click menus | `canvas_pan_zoom_menus`: middle-drag pans, a flick glides on and stops by itself (Esc stops it at once; none with reduced motion), Ctrl+wheel zooms in, **Show all nodes** recentres with zoom capped at 100 %, a box drag moves it in the document, right-click menu, double-click opens the new-session dialog | steps "mouse drag moves a session together with its subagent", "right-click opens a context menu; toolbar zoom buttons"; canvas-core model tests (pixel hit testing, continuous zoom, fitting visible boxes/groups/decks, pan cancellation and empty canvas; `glide` tests: momentum direction, cap and slow-down, frame-rate independence, stopping on input, only background pans, reduced motion) | ✓ |
| 2 | Status (working, needs you, done · review, idle, stopped, failed, open elsewhere), OSC 9;4, background subagents, Esc not counted as done | `status_from_history`: all seven states plus a background subagent from synthetic histories; an `abort` turns working into idle, not done · review | step "progress (OSC 9;4), title and bell reach the canvas"; `status.rs` tests | ✓ |
| 3 | Attention borders and tints; active-terminal halo (heavy while typing) | `attention_and_halo`: needs you and review boxes carry attention colours; the halo is heavy while typing | step "the active terminal's box gets a halo" | ✓ |
| 4 | Colour-blind-safe palette; distinct icon and border per state | `state_icons_and_borders`: needs you, review and idle have three different borders; ◆ ✓ ○ ■ ✗ ↗ all drawn | canvas-gui `theme.rs` palette test | ✓ |
| 5 | Subagents from history (folded into a deck beneath their session by default, WI-022) | `subagent_boxes_from_history`: “Parity explorer” box from `subagent.started` in the canvas file (no place of its own yet: it arrived folded), drawn as “With agents”'s slim “1 subagent” deck, working while it runs, with its edge; no box of its own on the canvas | canvas-model `visual` tests (old canvases fold, decks, layout), canvas-core `visual` tests (new boxes arrive folded and get a slot when first shown) | ✓ |
| 6 | Subsessions: `S`, fork or fresh, dotted connector; spawned-subsession detection | `subsession_with_dashed_edge`: `S` → dialog → linked child, connector added | steps "subsession linked to its parent", "a Copilot session started inside a session's terminal appears as its linked subsession" | ✓ |
| 7 | Looping sessions: loopback arc with a moving dot, `↻ every`, sidebar ↻, pulsing ↻ | `loop_from_session_schedule`: `↻ every 15m`, an arc, sidebar ↻ | step "repeating automations get the animated loopback" (the dot moves between frames) | ✓ |
| 7b | **New:** a smooth, lively loop dot from the clock; reduced motion keeps a still indicator | `loop_dot_smooth_and_reduced_motion`: five PNG captures 90 ms apart each show the lit dot within 3 px of where the clock put it (the hooks report it per capture), the lap advances with the time between captures and the dot moves a little every time, never jumps; the loop layer redrawn at its pace while in view (its own loop frames plus the full frames other animations need: at least 65 % of 2 s / its interval), no loop frames once every loop is scrolled away; with reduced motion no dot, two captures of the arc identical, no loop frames, `↻ every 15m` kept | canvas-core `loops` tests (continuity across frames and laps, frame-rate independence, cell stepping, grow-in and fade-out, TUI redraws only for visible dots), canvas-gui `the_loop_dot_glides_along_the_curve_at_any_frame_rate`; snapshots `looping-sessions`, `looping-sessions-reduced-motion`; `bench --client gui --loops` | ✓ |
| 8 | MCP picker with presets (Ctrl+S, ←/→, Del), per session and resume | `mcp_picker_preset_and_launch`: servers from the CLI, none selected, Ctrl+S saves preset “Parity none”, the launch passes `--disable-mcp-server` twice (the stand-in prints them), preset and per-session list saved | | ✓ |
| 9 | Groups: select, `G`, move by title, resize ◢, nested, geometric membership, ✎ notes and ☑ todos chips, launch defaults | `group_move_resize_chips`: the ✎ Notes and ☑ + chips (clickable), ◢ resize, move by title with its members | steps "right-drag rubber-band selection, then G groups", "a new group asks for its folder" | ✓ |
| 10 | Sidebar tree, `/` filter, click flies to the box, group rows tile terminals | `sidebar_filter_click_and_group`: `/Loop` filters, a sidebar click selects the box and moves the canvas to it | step "sidebar click reveals and views a session" | ✓ |
| 11 | Terminals: ConPTY, single or 4 tiles (`T`), survive client exit | `tiles_single_and_tiled` (`T` toggles), `detach_and_reattach` (a new GUI shows the shell's earlier output, then detaches cleanly) | steps "Enter opens a real native terminal", "tiled view", "detach", "reattach" | ✓ |
| 12 | Wheel rules (mouse mode, PgUp/PgDn when mouse off, scrollback, Shift forces scrollback) | `wheel_scrollback_and_shift`: 120 lines, the wheel scrolls back, typing returns to the live screen | canvas-gui `input.rs` wheel notch tests; canvas-host wheel/click encoding tables | ✓ |
| 13 | User-message tint (navy background, ▎ bar) | `user_message_tint_png`: the stand-in Copilot's message row is navy (30, 44, 70) in the PNG, the reply row is not | canvas-termview `user_message_rows` test | ✓ |
| 14 | Copy and paste: drag-select (soft-wrap rejoin), copy on release, Ctrl+C with selection, Ctrl+Shift+C, Ctrl+V, pane menu | `copy_on_release_and_paste`: a drag copies `COPY_PROBE_TEXT` on release; Ctrl+V pastes; a drag over a soft-wrapped line, a hard break, a blank line and indentation copies exactly that text; a two-line paste into PowerShell runs nothing; a four-line paste (blank line, indentation, trailing line break) and IME text with a line break reach the paste probe as one bracketed paste with no Enter | canvas-termview selection tests (engine wrap state, full-screen rows, Copilot's scrollbar), canvas-host paste encoding (both engines, every mode), canvas-core `coalesce` and paste tests; the 28-step self-test runs the same check in the TUI and GUI | ✓ |
| 15 | Image paste (Alt+V / Ctrl+V): direct Ctrl+V; show the image | `paste_image_preview`: with an image on the clipboard Ctrl+V goes to the agent and a 64 × 40 thumbnail appears (pixel checked) | canvas-gui `clipimage.rs` (DIB decoding), `images.rs` (thumbnails) | ✓ |
| 16 | Click forwarding in Copilot's mouse mode; drags still select | `click_forwarding_and_drag_select`: the stand-in receives `ESC[<0;7;…M`; a drag selects and copies | | ✓ |
| 17 | Title in the pane header, taskbar state mirror, bell, OSC 777 | `title_progress_bell_toasts`: OSC 0 title in the header; taskbar paused (yellow) while one needs you, busy once answered, paused again; bell counted; OSC 777 and a needs-you transition become toasts (in the Action Center) | step "progress (OSC 9;4), title and bell"; canvas-gui `platform.rs` tests | ✓ |
| 18 | Links: URLs, OSC 8, Windows paths; Ctrl+click; nothing executed | `links_osc8_and_paths`: OSC 8 and plain-path links found; Ctrl+click on a missing path gives a notice | step "links in terminals: plain click focuses, Ctrl+click opens"; canvas-gui tiles link tests | ✓ |
| 19 | New session dialog: name, folder (type, Browse…, Recent ▾), Git detection, worktree, first prompt, MCP row, agent | `new_session_dialog_fields`: every field, the Git probe, the folder browser | steps "new session dialog", "folder browser + Git detection + new worktree session" | ✓ |
| 20 | Import (filter, multi-select, grouping); VS Code sync (+ Ctrl+Z, auto sync) | `import_dialog_and_vscode_view`: filter, Ctrl+A, import two sessions; the VS Code view | step "VS Code view offers sync and migration" | ✓ |
| 21 | Activity bar views (Alt+1…9) | `activity_views_alt_1_to_9`: all nine titles | steps for Automations, Settings, VS Code and Reports views | ✓ |
| 22 | Copilot defaults, group overrides, `copilot config` list and edit | `settings_defaults_and_config`: `config --list` shown, the default model edited and saved | step "Settings view edits Copilot launch defaults" | ✓ |
| 23 | Plugins, MCP, skills: list, toggle, remove (confirmed), add, install in a terminal; secrets hidden | `mcp_view_lists_and_toggles`: servers listed from the CLI, Space toggles through it, the plugins view loads | canvas-agents `mcp_listing_shapes_are_normalized_without_secrets` | ✓ |
| 24 | Migrate from VS Code (preview, tick, backup, redacted log) | `migration_preview_dialog`: the read-only preview opens and closes | canvas-agents migrate tests | ✓ |
| 25 | Passive sessions: templates, panel, dashed ⟲ boxes, ✉ badge, pulses, idle-only messages, mute, rollover, budgets | `passive_create_and_panel`: the empty group's ⟲ + passive chip → its passive panel's empty state → *+ Create a passive session for this group…* → template → name → panel, rules scaffolded, a dashed ⟲ box, the chip reads ⟲ 1 (the group menu's *New passive session…*: the TUI self-test probe) | canvas-core passive tests | ✓ |
| 26 | Automations: families, failure pulse, toolbar badge, info and health page, run, enable, ask doctor, outputs | `automations_view_and_details`: listed with prompt, schedule and runs | steps "Automations view lists Copilot automations", "clicking an automation … Space toggles it" | ✓ |
| 27 | Copilot automations (`data.db`) and session schedules | `automation_boxes_and_session_loops`: three automation boxes; the session schedule loops | | ✓ |
| 28 | Idle auto-stop, priority manager, upgrade with resume, protocol-mismatch tolerance, crash log | `crash_log_and_build_match`: no update prompt for a current service; a crash is written to `crash.log` and the sessions keep running (idle stop, priorities and upgrades are the service's, shared with the TUI) | `upgrade_probe` (manual), canvas-host tests | ✓ |
| 29 | Keyboard map, F6 / Ctrl+] to leave a terminal, Q / Ctrl+Q quit dialog, `?` help | `help_f6_and_quit_dialog` | steps "Ctrl+] returns keyboard focus", "detach", "stop all and quit" | ✓ |
| 30 | `--demo` mode | `demo_mode_canvas` | the whole 45-step self-test runs in `--demo` (its canvas-wide agents, pull request, review and todo CLI steps open a canvas of its own) | ✓ |
| 31 | `--perf` overlay (GPU timings, round-trip time) | `perf_overlay_rtt`: frame timings and service round trips measured | `bench --client gui` | ✓ |
| 32 | CLI (service, doctor, bench, events, emit, send, status, followup, automations) | `cli_unchanged` — **no front end:** the same CLI binary for both clients | existing CLI tests | ✓ |
| 33 | **New:** remote mode (pair, reconnect, resync, offline input blocking) | `remote_loopback_gui_and_upload`: the GUI over loopback TCP; a dropped file is uploaded to the service's machine and its path pasted | `selftest --client gui --remote loopback` (45 steps over TCP); `selftest --remote faults` (the reconnect core both clients share) | ✓ |
| 34 | **New:** view versus start: a click (canvas, sidebar) only views a box (its live terminal or its details panel); double-click, Enter, the panel's Start / Resume and menu items start it; group rows never launch | `view_vs_start`: a click on a stopped session shows its details with Resume and launches nothing (service log, no tile); a double-click, Enter and the Resume button each resume one (the stand-in Copilot); a group row shows running terminals and launches nothing | steps "a click views a stopped session without launching it", "a double-click on the box starts it", "the details panel's Start button starts a session"; canvas-core `interact` tests | ✓ |
| 35 | **New:** transcript inspector for session and subagent boxes (live while they run); a finished child resumes as its own linked subsession, never beside a writer | `inspector_and_child_resume`: a Copilot session's transcript and, through its deck (a click lists the session's subagents in its details panel, the row's Inspect), a subagent's note; the OpenCode session's children arrive folded (a deck of 2), `K` shows them (each gets a slot of its own); an OpenCode child (synthetic database) that works, whose inspector shows a new line written while it is open; with its parent running (stand-in OpenCode) a finished child offers no resume; parent stopped: **Resume as subsession** adds a linked box and the service resumes it with `-s <child>` | canvas-agents `transcript` tests (Copilot with subagent events, OpenCode with a child, Claude Code with a subagent file), canvas-core `inspect` tests (resume rules, a refused resume), `backend_contract` (local and remote answers match); `selftest agent --agent opencode --prompt` (inspect and resume a real child) | ✓ |
| 36 | **New:** removing a multi-selection: right-click on a selected box (or Del) removes its eligible boxes after a confirmation; sessions running in a canvas terminal stay; history is never deleted; Ctrl+Z restores them | `multi_remove_keeps_running`: a 3-box selection (one running) → menu “Remove 2 from canvas” → the confirmation names the running one → two boxes gone, their history files kept → Ctrl+Z brings them back | canvas-core `remove` tests (eligibility, subagents go with their session, subsessions unlinked, undo) | ✓ |
| 37 | **New:** a click on a group's empty background or its border selects the group (innermost; a few pixels outside the outline in the GUI); title drag, ◢ resize, the title's chips, background pan and double-click are unchanged | `group_background_and_border_select`: an empty background spot and a point 0.2 cells outside the outline each select the group (its panel, nothing launched); a background drag pans; title drag and ◢ still move and resize; the chips (✎ Notes) stay clickable; a double-click opens the new-session dialog | step "a click on a group's border selects the group" (heavy outline, its panel); canvas-core `interact` tests (both render modes, nested groups, gestures) | ✓ |
| 38 | **New:** recent sessions: a RECENT section atop the Sessions panel (newest first, shared by every window through the document); Ctrl+F6 / Ctrl+Shift+F6 (terminals too) and `]` / `[` view the next or previous one and never start it | `recent_sessions_and_walk_keys`: three sessions viewed in turn are listed newest first and saved in the canvas document; Ctrl+F6, Ctrl+F6, Ctrl+Shift+F6 select the one before, the one before that and back (details panels, a "Recent 2 of N" notice) while the list keeps its order; a second later the one reached is first; `]` and `[` on the canvas do the same; no launch in the service log, no terminal for any of them | step "recent sessions: Ctrl+F6 / Ctrl+Shift+F6 view recent sessions without starting them" (from a terminal, TUI and GUI); canvas-model `recent` tests (order, clock skew, walk, removal, document round trip and two windows merging), canvas-core `recent` tests (keys never launch, the walk settles after a second or on any other input, Ctrl+F6 in a terminal, `]` reaches the agent there, the RECENT rows hold still for a double-click); snapshot `sidebar-recent` | ✓ |
| 39 | **New:** the default model for new sessions: the new-session dialog shows the model it starts with and why (group override, remembered default, initial Claude Opus 5.5); a pick applies to that session and becomes its agent's remembered default (in a group with its own model, that session only); Settings shows and resets each agent's default; resumes keep a session's own model; 39b: still remembered after a restart | `remembered_default_model`, `remembered_model_after_restart`: the stand-in Claude Code starts with `claude-opus-5-5` (nothing chosen, nothing remembered); a model picked from the list starts the next session and is remembered and recorded on its box; a resumed older session keeps `--model claude-opus-5-5`; a Copilot resume carries no model; a group override is shown "from group", a pick there is for that session only, then reset; a restarted GUI still shows the remembered model | `selftest agent` and `selftest agent --agent claude` (the launch command line carries the default model); canvas-model `models` tests (resolution, migration of the old single model, round trip), canvas-agents launch tests (new versus resume per agent); snapshots `new-session-dialog`, group model and model picker | ✓ |
| 41 | **New:** the Reports inbox: sessions publish reports and short messages (`copilot-canvas report publish`); an unread count on the activity bar, a notice and a toast; the viewer formats Markdown and marks it (its thread) read at the service; viewing its session starts nothing | `reports_inbox_and_viewer`: a stopped session's environment publishes a Markdown brief (`--kind warn --pr`) and a queued message in a thread; ✉2 on the activity bar, "New report from Stopped one", a toast per report asked for (quiet while the window is in front); a click on the brief shows it formatted (bullets, a table with rules, a quote), ✉1, read in the service's `state.json`, the message still unread; *View “Stopped one”* closes the viewer and shows its details panel (Resume), the message reads with one click, nothing unread; the service log shows no launch | step "reports: a session publishes a Markdown report and a message …" (TUI and GUI, also over loopback); canvas-core `reports` tests (threads, read on open, never a launch, unread/delete/links through the service), canvas-agents `reports` tests (checks, queued then stored, refusals, retention), `backend_contract` (local and remote inbox, push only to `CAP_REPORTS` clients); snapshots `reports-inbox`, `report-viewer-markdown`, `report-viewer-html`; goldens `reports-inbox`, `report-viewer` | ✓ |
| 41b | **New, GUI only:** HTML reports in a WebView2 view with their own styles; scripts never run, nothing is fetched, navigation blocked (the TUI shows HTML as formatted text) | `html_report_locked_down`: an HTML report with a `<style>`, a `<script>`, an `onerror`, an external stylesheet, CSS and image URLs pointing at a local listener; the view shows it (the viewer says "HTML view: scripts off, nothing loaded from the network"); in the page: no script element, the image without `src`, the CSP meta, the heading magenta from the page's own CSS, the title unchanged; the view's capture has the magenta heading and the composed window (`PrintWindow`) has it only inside the viewer's content area; then an unsanitized page (`probe_raw`): its script element is there but does not run, its requests are answered 403, a `location.href` change is cancelled and the page stays; the listener got no connection; Esc hides the view | canvas-model `report_html` tests (sanitizer, CSP document), `richtext` tests (HTML as text) | ✓ |
| 41c | **New:** remote mode lists and opens reports through the service | `remote_reports_inbox`: a message published on the service's machine; a GUI over `--remote loopback` shows ✉1, opens it (`Request::ReportContent`) and it is read in the service's `state.json` | `selftest --client gui --remote loopback` (the reports step over TCP); `backend_contract` over loopback TCP | ✓ |
| 41d | **New, GUI only:** HTML notes in the WebView2 view, served from `https://notes.canvas.invalid/` (the note's folder read through `Request::ReadBytes`: relative images and styles work, also remotely); scripts off by default (the note sanitized), allowed per file (a sandboxed view of their own, offline); `#note:` / `#todo:` links navigate the app; *Print / PDF* (the TUI shows HTML notes as formatted text) | `html_note_pane`: `page.html` in the parity group's `human-outputs` with a relative `style.css` (a magenta heading), a relative `pixel.png` and a script that renames the page and fetches from a local listener; opened from the explorer it loads from the notes origin (`location.href`) in the plain view: its style applies, the 4 × 4 image loads, both files are served (`served_request_uris`), no script is kept; *Scripts: off* → on: the file is listed in the canvas (`notebook_scripts`), the scripts view shows it and its script ran (the title), still styled; off again: the plain view, nothing runs; a click on `#todo:PAR-001` in the page opens its card in Tasks, Alt+← comes back, `#note:summary` opens the note (2 app links); the `webview` hook's `print` opens the print dialog without an error notice; the listener got no connection | canvas-gui `webpane` tests (the notes origin's URLs, files of the note's folder only (links resolve under its root), replies with the policy, fragments and link actions, `fragment_allowed` and `AppLinks` (one app link per click into a scripts page), global keys, the bounded request queue, the browser's arguments, PDFs never written over), canvas-model `report_html::notes` tests (the sanitizer keeps folder links, no markup through `<svg><style>`, the two policies); `backend_contract` (`ReadBytes` local and remote) | ✓ |
| 44 | **New:** undo canvas movement: Ctrl+Z takes back this window's latest layout change, one completed gesture per step (a drag of boxes, a stack or a group with everything that moved with it, a ◢ resize, *Fit to members*, a burst of arrow keys), to the exact places and sizes; successive undos go further back through moves, removals and syncs in one history; Ctrl+Y / Ctrl+Shift+Z redo; nothing starts or stops; a terminal with the keyboard gets Ctrl+Z itself | `layout_undo_redo`, on a canvas of its own (a group with a parent and its subagent box, a nested group with a subsession and its subagent box (still folded into its deck), a shell, another box): the group dragged by its title (every box, the folded one too, and the nested group moved by the same delta) and resized by ◢; in the shell's terminal the paste probe receives Ctrl+Z (`KEY \x1a`) and the canvas file keeps both changes; on the canvas Ctrl+Z restores the size, a second Ctrl+Z every place exactly (the canvas file), Ctrl+Y moves it again and Ctrl+Z back; no launch in the service log | step "Ctrl+Z undoes a group drag exactly … in a terminal Ctrl+Z reaches the program" (TUI and GUI, also over `--remote loopback`); canvas-core `undo` tests (box, group with members, children and nested groups, resize, multi-selection moved once, arrow-key bursts, removals and syncs in one history, the 100-step limit, items removed or moved by another window since, the undo as a document edit, Ctrl+Z in a terminal, a dialog and the sidebar filter, *Fit to members*, stacks, folded subagent boxes carried by a box or group drag and put back by its undo, folding never an undo step), canvas-model `a_selection_with_its_children_moves_each_box_once` | ✓ |
| 45 | **New:** area selection takes subsessions and subagent boxes like sessions: Shift-drag (and right-drag) selects every visible box the band touches as drawn, parents and children together, at every zoom; boxes in a deck (stacked, folded) and hidden ones are left out; a Shift-drag that starts on a box selects by area and never drags or starts it; moving a mixed selection moves each box once | `subsession_area_selection`, on the canvas of row 44 at 100 % and at 50 %: `K` first shows the parent's subagent box at its saved place (folded by default); a Shift-drag from on top of the parent over its subagent box selects exactly those two (the tree's selection), a second one adds the subsession; no box moved, nothing launched; dragging the parent moves the parent, its subagent box, the subsession and the subsession's (folded) subagent box by one and the same delta in the canvas file, the other boxes stay; Ctrl+Z puts them back; with `H` and `J` (hiding subsessions and subagent boxes) a band over the whole group leaves the subsession and both subagent boxes out | step "K shows a session's folded subagent box at its saved place; an area selection over the session and the box selects both at 100 % and 50 %; moving them moves the subagent once; K folds it again" (a right-drag in the TUI and the GUI, also over `--remote loopback`); canvas-core `selection_tests` (every zoom of both render modes and a continuous 70 % with a fractional origin, the band against the drawn cells at 25 %, hidden, stacked and folded boxes, Shift- and Ctrl-clicks, a Shift-double-click never starts, right-click menus, removing a mixed selection and its undo), `undo` `a_mixed_selection_moves_each_box_once`; snapshots `selection-subsessions-100`, `selection-subsessions-50` | ✓ |
| 46 | **New (WI-022):** subagent boxes fold into one slim deck beneath their session by default (also on canvases saved before; *Settings › Subagents* for the default; a session's own choice wins), showing the most important state of what it holds; `K`, the menus or a double-click show them at exactly their saved places; the choice is saved in the canvas; `J` hides them too; nothing starts | `subagents_folded_by_default`, on a canvas of its own (a live session with a running subagent in its history; two subagent boxes saved at places of their own by a build that did not fold, no choice saved): one “2 subagents” deck with a card behind it, working (“1 of 2”), no box of their own, their saved places kept; a click on the deck lists both in the session's details (SUBAGENTS 2); `K` shows them (a transition frame seen) exactly at their saved places (the canvas file and the drawn offsets from the session); the GUI restarted: still shown; `J` hides them (◇ 2 hidden on the session and in the toolbar's subagent counter) and shows them; `K` folds them again, places kept; no launch in the service log; PNG captures `subagent-deck`, `subagent-deck-50`, `subagent-deck-details`, `subagent-deck-unfolded` | step "K shows a session's folded subagent box …" (TUI and GUI, also over `--remote loopback`); canvas-model `visual` tests (old canvases fold and round-trip the choices, a shared field, one deck per session, folded boxes take no room, *Fit to members* shrinks to what shows, group membership and group drags follow the session), canvas-core `visual` tests (the slim deck and its status, a click views and a double-click unfolds, K with subagent boxes only, subsessions only and both, the menus, new boxes arrive folded and get a slot and a grown group when first shown, the Settings default and per-session choices, H hides subsessions and J subagent boxes, each on its own, with counts, effects on the deck, GPU transitions and reduced motion), `undo` and `selection_tests` (above); snapshots `subagent-decks`, `subagent-decks-unfolded`, `subagent-deck-details` | ✓ |
| 47 | **New:** spaces: the toolbar's ◫ Canvas / ✎ Notebook / ☑ Tasks tabs and Ctrl+1/2/3 switch the main area (and slot 1 of the activity bar); Go to (Ctrl+P) searches groups, sessions, notes and todos and jumps there without starting anything; back and forward through the window's history (Alt+← / Alt+→, Alt+, / Alt+., the mouse's back and forward buttons) | `spaces_goto_and_history`: three toolbar tab targets; the ✎ tab, Ctrl+3, Ctrl+2, Ctrl+1; Go to a note (“Parity summary”), a todo (PAR-002: its details), a session (“Stopped one”: viewed, selected), the group (selected); Alt+← twice (the session, the todo), Alt+, (the note), Alt+. and Alt+→ forward again; no launch in the service log | steps "spaces: the toolbar's tabs and Ctrl+1/2/3 …" and "Go to (Ctrl+P) jumps to a note, a todo and a session …" (TUI and GUI, also over `--remote loopback`; the TUI gets Ctrl+digits as win32-input-mode keys through ConPTY); canvas-core `spaces` tests (history, fuzzy matching, Go to's providers, keys, never a launch), canvas-gui `input` test (mouse back / forward); hardening `ui_automation_tree_seen_by_a_uia_client` (three toolbar TabItems, the Notebook tab pressed through UIA); snapshot `goto-dialog`; golden screenshots `tasks-board`, `notebook-source` | ✓ |
| 48 | **New:** the Notebook: the explorer (the notebook root; a group's folder outside it as a root of its own, `◫`; the canvas-wide folder; Reports), ● for you / ◇ for agents, the Human \| All \| AI filter ("N AI hidden"), the reader (Rendered in the HTML view; Source and Live in the editor), typing and saving (Ctrl+S, autosave), quick capture (Alt+N), changes on disk (a clean note reloads, an edited one shows *Keep mine* / *Take theirs*) | `notebook_explorer_reader_editor`: `ai-docs/plan.md` (`audience: ai`) and `human-outputs/summary.md` written into the parity group's folder; its ✎ Notes chip shows the folder as `◫ Parity group` with ◇ plan.md and ● summary.md; *Human* hides plan.md ("AI hidden"), *All* shows it; summary.md Rendered in the HTML view (its heading, `[[plan]]` and `PAR-001` as links, no script); Ctrl+E from the explorer → Source with the keyboard in the editor; typed text saved by Ctrl+S (the file gains `author: mixed` and the caret stays on its line); Live hides the heading's `#`; a change on disk reloads the clean note; typing while it changes on disk → the conflict banner, *Take theirs* keeps their file; Alt+N writes a note into `<home>/notebook/Inbox`, in Source mode; nothing launched | steps "notebook: Alt+N writes a quick note into the Inbox …" and "notebook: Go to flies to a group; its ✎ Notes chip …" (TUI and GUI, also over `--remote loopback`); canvas-core `notebook` tests (tree and filters, links, saving and autosave, conflicts, quick capture, the caret after front matter the app wrote, Ctrl+E's keyboard) and `editor` tests, canvas-model `notebook` tests, canvas-agents notebook and `note` CLI tests, `backend_contract` (the notebook requests, local and remote); snapshots `notebook-space`, `notebook-reader`, `notebook-source`, `notebook-human-filter`, `notebook-conflict`, `notebook-missing` | ✓ |
| 48b | **New:** the Notebook and Tasks in remote mode (`--remote loopback`): folders listed, notes read and written, the todo book, all through the service | `remote_notebook_and_tasks`: a GUI over loopback TCP; the ✎ Notes chip lists the group's folder, `remote.md` opens in Source (read through the service) and Ctrl+S writes it on the service's machine; Ctrl+3 shows the board and its todos from the service's book; nothing launched | `selftest --client gui --remote loopback` and `selftest --remote loopback` (every notebook and Tasks step over TCP); `backend_contract` over loopback TCP | ✓ |
| 49 | **New:** Tasks: the todo canvas (board frames, columns, cards) of the book the service keeps; a todo's details; A accepts a proposal; a card dragged to another column changes its status; changes apply at once and go to the service | `tasks_canvas_details_accept_drag`: `copilot-canvas todo board add`, `board link`, `add` ×2 as you and `todo propose` from a canvas session's environment (an agent's own idea: Proposed); Ctrl+3 shows “Parity board · PAR” with PAR-001…003; a click on PAR-003 shows its details, A accepts it (Backlog in the service's `book.json`); PAR-002 dragged from Backlog into Todo (Todo in `book.json`; its details say Todo); Esc closes the details, ↓ selects the first card in view, Enter opens its details; nothing launched | steps "tasks: a box's ☑ WI-002 badge opens its card; … A accepts a proposal; a card dragged …" (the demo book) and "todos through the CLI …" (TUI and GUI, also over `--remote loopback`); canvas-core `tasks` tests (layout at three zooms, hit testing, drops and ranks, keys, optimistic changes replaced by the service's book, demo changes kept, one-cell glyphs), canvas-model `todos` tests (operations, policy, import, mirror, export), canvas-agents `todos_cli` and `service::todos` tests; snapshots `tasks-space`, `tasks-demo-100`, `tasks-demo-50`, `tasks-demo-25`, `tasks-details`, `tasks-link-boards` | ✓ |
| 50 | **New:** links between the spaces on the canvas: a group's ✎ Notes chip and its ☑ chip (open · in progress · proposed; gold while proposals wait; `☑ +` without a board) and a session's ☑ ID badge (the todo it claimed) open their place in the Notebook or Tasks; nothing starts | `chips_and_badges_navigate`: `todo claim PAR-001` from the stopped session's environment and a second proposal; Go to the session: its box has the `PAR-001` badge, the group's ☑ chip is gold; a click on the badge opens PAR-001's details in Tasks; the proposal declined (CLI): the chip is no longer gold and opens the group's board; the ✎ Notes chip opens the folder in the Notebook; no launch | steps "tasks: a box's ☑ WI-002 badge opens its card; the group's ☑ chip opens its board …" and "todos through the CLI … the group's ☑ chip counts them … the session's ☑ badge opens the card" (TUI and GUI, also over `--remote loopback`); canvas-core `folders` tests (chips and how they fit), `tasks` test `group_chips_and_session_badges_come_from_the_book`; snapshot `canvas-spaces-chips`; golden screenshots `demo-canvas`, `demo-zoom-50` | ✓ |
| 51 | **New (WI-023):** the canvas's dot grid breathes gently while at least one session works (a slow 3 s brightness pulse, about 40 % brighter and a touch bluer at its top; it fades in and out over 0.8 s and follows the clock); reduced motion, effects off and *Settings › Dot grid pulse* off keep it still; only the grid's layer is repainted for it (≤ 15 frames/s of its own), nothing when nothing works | `grid_pulse_while_working`, on a canvas of its own (one session working from its synthetic history, a tool running): reduced motion gives the reference captures (the dots' normal colour; two captures half a breath apart identical, no grid repaints), effects off the same; motion on: captures scheduled at the top and the bottom of a breath on the animation clock the GUI shares (each `png` reply carries its `clock_ms` and the grid's level, which must be the clock's, so nothing depends on the time between requests): at the top every sampled dot (24, clear of boxes and groups) is lighter and a little bluer within the pulse's range, at the bottom the normal colour; in 2 s while working only grid repaints for it (≤ 15 frames of its own a second; it may also ride on the sandbox's loop frames) and no extra full frames, none with the pulse off; *Settings › Dot grid pulse* (a click on its row) off: saved in `settings.json`, the normal colour; on again; the session stops (an `abort`): the pulse fades out, then no frames at all and the normal colour | canvas-core `pulse` tests (bounded, periodic, the normal colour at the bottom of a breath, continuous and the same at any frame rate, fades turning around mid-way, a switch between working sessions changing nothing, subagents, reduced motion, effects off, the setting off and `freeze`, the TUI's steps and its return at once, grid frames only while it shows, the Settings row), canvas-gui `the_grid_dots_breathe_with_the_pulse_and_hold_their_colour_without_it`; snapshot `grid-pulse`; `bench --client gui --pulse` | ✓ |
| 52 | **New:** the ✦ Skills view lists every skill a canvas session can get, by source (CANVAS, GROUP SKILL FOLDERS, PROJECTS, PERSONAL, CANVAS LIBRARY), each with the agents that load it; the *Agent* row shows one agent at a time; group skill folders (the group menu's *Skill folders…*, checked on the service's machine) reach every session of the group with all three agents; viewing starts nothing | `skills_view_projects_and_group_folders`: a project skill `pa-proj` in the parity sessions' folder (`.claude/skills`) and a skill folder beside the sandbox (`pa-grp`); the parity group's *Skill folders…* refuses a folder that is not there ("does not exist on the service's machine", checked in the background), adds this one (the canvas file has it; the editor counts 1 skill); Alt+5: CANVAS (`canvas-events`), GROUP SKILL FOLDERS with the group and `pa-grp`, PROJECTS with `pa-proj`, both rows tagged `all`; `A` shows each installed agent alone (both skills still listed; PERSONAL and CANVAS LIBRARY on the screen of at least one: with every agent the owner's own personal skills can push them below it), then every agent again; no launch in the service log; the folder is taken off again (Del in the editor) and only the files the row wrote are removed | canvas-model `skills` tests (each agent's folders, project roots, scanning and its bound, sources and agents, the 40-project cap, merging, filtering, same names, group plugins: links, full paths, SKILL.md copied first, removal only inside their root), `launch` tests (every launch in a group carries the folders; the postcard trip; the entry taken out whatever its case), `doc` (union of the canvas's and the group's, verbatim paths), `bus` and `rpc` tests; canvas-agents `service` (every agent's command line with a group plugin, interactive and passive; an exit after a relaunch keeps the relaunch's plugin) and `passive` tests; canvas-core `skills` and `skill_folders` tests (rows, filter, actions, a click shows and a second opens, the editor never waits for the service, at most 20 a list, Settings, routing); `backend_contract` (`SkillSources` locally and remotely); snapshots `skills-view`, `group-folder-chips`, `canvas-spaces-chips` (the group details' SKILLS) | not run yet |
| 53 | **New (WI-024):** `H` hides subsessions (with their subagent boxes) and `J` every subagent box (folded ones too), each on its own and together: two switches of the window (kept in its preferences), each with its canvas-menu item (*Hide/Show subsessions*, *Hide/Show subagents*) and its toolbar counter (**↳ N hidden**, **◇ N hidden**); each parent's marker counts what is hidden under it by kind; a deck keeps what is not hidden; nothing moves, nothing starts | `hide_switches_each_kind`, on a canvas of its own (a parent with a subagent box and a subsession with one, all shown): `J` hides both subagent boxes and keeps the subsession (◇ 1 hidden on each session, the toolbar's **◇ 2 hidden** alone; the preferences file says `"hide_subagents": true`); `H` as well hides the subsession (**⋯ 1 · ◇ 2 hidden** on the parent, **↳ 1 hidden** and **◇ 2 hidden** in the toolbar); `J` off brings the parent's subagent box back while the subsession's stays hidden with it (**↳ 1 hidden** alone); `H` off: every box back at its place in the canvas file; no launch in the service log; PNG capture `hide-both` | canvas-model `visual` `hiding_maps_each_box_to_its_nearest_visible_ancestor_by_switch`; canvas-core `visual` tests (`h_hides_subsessions_and_j_subagent_boxes_each_on_its_own`: markers, counters, menus, decks, places, no undo step; `a_deck_hides_what_is_hidden_and_shows_the_rest`; `hide_switches_are_kept_with_the_windows_view_and_old_prefs_load`), `selection_tests` (hidden boxes out of area selection with `H`, then `J`); snapshots `hidden-subsessions`, `hidden-subagents` | ✓ |
| 54 | **New (WI-025):** right-click a subsession or a subagent box › *Hide* (*Hide N* on a multi-selection; a deck's *Hide ◇ name*; *Hide* per row of a session's SUBAGENTS list) puts it into the canvas's hidden area (`Node::hidden`, the canvas file, every window): not drawn, clicked or area-selected, moving with its parent, counted on its parent's marker by kind in the colour of the most important hidden state and on the toolbar's counters; a click on the marker (or the parent's *Show hidden (N)…*) lists them with *Show* and *Show all*, the counters list the whole canvas's by parent; the sidebar keeps them, dimmed and marked *hidden*; *Show* puts a box back exactly where it was; never an undo step, nothing starts | `hide_one_box_and_show_it`, on the canvas of row 53: *Hide* from the subsession's menu (the canvas file says hidden; its subagent box goes with it: **⋯ 1 · ◇ 1** on the parent) and from the parent's subagent box's menu (**⋯ 1 · ◇ 2 hidden**, **↳ 1 hidden** and **◇ 2 hidden** in the toolbar, the sidebar row marked *hidden*); a click on the marker lists *Show all (2)*, *Show ↳ Hide child*, *Show ◇ Hide helper*; *Show ↳ Hide child* puts it back at its place (the canvas file and the drawn offset from the parent) with its subagent box; the toolbar's **◇ 1 hidden** lists the rest (the switches on top) and *Show ◇ Hide helper* brings it back; no box moved, no launch in the service log; PNG captures `hide-area`, `hide-area-list` | canvas-model `visual` `boxes_hidden_on_their_own_take_their_subtree_and_move_with_their_parent` (the subtree, the visible ancestor, no room taken, moving with the parent and its group, decks, the document round trip and old canvases); canvas-core `hidden_area_tests` (menu → marker → Show in both render modes, the edit that goes out, multi-selection *Hide 2* and the toolbar's list grouped by parent, a hidden box that needs you or fails colours the marker, a folded subagent box hidden from its deck and from the SUBAGENTS list, the sidebar's mark and a click that only views, moving with the parent and one undo, arrow keys); snapshots `hidden-area-list`, `subagent-deck-details` (the rows' *Hide*), `hidden-subsessions` / `hidden-subagents` (the sidebar's *hidden*) | ✓ |

| 40 | **New:** canvas-wide agents: the top bar's **◎ Agents** (a management panel: each agent's state and controls, the canvas skills, the group passive sessions for contrast), the *Canvas agents* frame (◎ chips) and the sidebar show one agent per box; templates (PR coordinator, Cross-group digest, Custom); canvas skills given per agent; viewing one never starts it | `canvas_agents_one_agent_every_entry`: ◎ Agents → *+ New canvas agent* → *PR coordinator* → named: a dashed ◎ box in the new frame, its files under `<home>/canvas-agents/<canvas id>/agents/<slug>`, routed as canvas-wide for this canvas id; the panel's row, a click on the box and the sidebar row select the same box (its panel); a double-click launches nothing; a skill of the canvas library given with *Skills…* (document, routing, panel) | step "canvas-wide agents: ◎ Agents opens their panel; a new agent is one box in the Canvas agents frame …" (TUI and GUI, and over `--remote loopback`); canvas-core `canvas_wide` tests (state machine, routing per canvas id, disable, pause, stop, frame, group filter), canvas-model `canvas_wide` and `bus` tests (folders per home and canvas, skills, plugin, prompt, subscribers across groups, isolation, permissions), canvas-agents backend, CLI and provider tests; snapshots `canvas-agents-panel`, `canvas-agent-detail` | ✓ |
| 40b | **New:** a canvas-wide agent runs on events of every group of its canvas (never another canvas's), with its skills delivered to the agent CLI; Pause, Run now, Stop run, Disable | `canvas_agent_runs_across_groups`: a `pr.published` from a session of the parity group runs the agent with the stand-in Copilot (`-p`, its skills plugin as a second `--plugin-dir`), which reports `fake run of pr.published · skills: parity-skill` through `copilot-canvas status`; paused, an event of the canvas and one from a session no canvas routes start nothing (12 s); *Run now* then *Stop run* ends the run as stopped with no failure; disabled, it is no longer routed and an event starts nothing | canvas-agents `passive` tests (addressed events, effects on any box for canvas-wide agents, run folders, state kept for another canvas), canvas-model `bus` tests | ✓ |
| 42 | **New:** pull request coordination: a session hands a pull request off (`copilot-canvas pr track`); the canvas checks it read-only with `gh` / `az`; new review comments become one follow-up for its owner from the PR coordinator, queued while the owner works and typed in when it is idle; Reports shows queued then delivered; a merged pull request is never checked again; ◎ Agents › Pull requests lists it all | `pr_handoff_follow_up_and_end`: a new session (the stand-in agent) and a *PR coordinator* agent (PR coordination on); the stand-in `gh` serves a pull request with an old comment; the owner's `pr track` answers "Tracking parity/app#42 …"; the first check is the baseline (no event); three new comments while the owner is typed into → one `pr.comment.new` → the coordinator (the stand-in following the `canvas-prs` contract) dispatches with its key → *queued*; idle 20 s later *delivered*, and `[From Parity PRs via Copilot Canvas] PR parity/app#42 …` is in the owner's terminal; still one follow-up after more checks; the thread's reports are queued, delivered; merged → *ok* "merged: monitoring stopped" and no `gh` call for it afterwards, none of the calls a write; the panel lists it merged | step "pull requests: a hand-off is tracked and polled …" (TUI and GUI, and over `--remote loopback`: a blocked follow-up with Resume owner / Reassign / Dismiss, dismissed); canvas-model `prs` tests (links, registry, dedupe, `gh`/`az` parsing, watermarks, end states, back-off, permissions), canvas-agents `prs_cli` and `service::prs` tests (requests, reach, monitors, CLI failures), canvas-core `prs` test; snapshots `prs-panel`, `canvas-agent-detail` | ✓ |
| 43 | **New:** review follow-ups: a review session hands a finished review off (`copilot-canvas review handoff`, with private notes); the *Review follow-up* agent drafts the message to the author, which waits for your approval; *Approve & send*; it is sent through the configured channel and recorded; every hour (10 s here) the agent reads the author's conversation; a re-review request is recorded once, tied to the pull request and delivered to the review session | `review_handoff_message_and_request`: a new review session (the stand-in agent) and a *Review follow-up* agent whose *Follow-ups…* sets the `outbox` channel (a folder: the stand-in messaging channel); the hand-off answers "recorded for Ana Parity"; the stand-in drafts (link, outcome, no notes) and the draft waits (nothing in the outbox; a warn report "Approve the message to Ana Parity …"); *Approve & send* in ◎ Agents › Review follow-ups; the stand-in sends it into the outbox (summary and link, not the notes) and records it; the conversation is watched; the author's reply asks for a re-review (and one does not); on the check the request is recorded once and delivered to the review session (`[From Review follow-ups via Copilot Canvas] Ana Parity asks for a re-review of PR parity/lib#7 …` in its terminal); a later check adds no request; Reports has warn, ok, queued, delivered | step "review follow-ups: a hand-off; a draft quoting private notes is refused …" (TUI and GUI, and over `--remote loopback`: a blocked request with Resume reviewer, Check now, Pause checks); canvas-model `reviews` tests, canvas-agents `reviews_cli` and `service::reviews` tests, canvas-core `review_followups` test; snapshot `review-followups` | ✓ |

No row is TUI-only: every feature of the TUI exists in the GUI. Row 32 is
not a front-end feature.

## New in the GUI (GUI-only)

| | Feature | How it is tested | ✓ |
|---|---|---|---|
| G1 | **Inline images.** Kitty graphics in Ghostty (64 MiB per terminal, inline data only); iTerm2 inline images (`OSC 1337;File=`, which ConPTY passes through) become Kitty placements in the host; `ServiceMsg::Image` (PNG, once per client and image) and `Placements` go only to `CAP_IMAGES` clients; textures per image, drawn below or above the text by z | `kitty_image_pixels_png`: `copilot-canvas kitty-demo` in a shell; the image is at the expected 8 × 2 cells and its magenta and green halves are in the PNG; a client without `CAP_IMAGES` (as the TUI connects) gets frames but no images. Unit: ghostty Kitty tests, `inline.rs`, protocol `shaped_for`, termview, `images.rs` | ✓ |
| G2 | **Taskbar, flashing, toasts.** ITaskbarList3 mirrors the canvas (busy / paused); FlashWindowEx for bells and needs-you in the background; WinRT toasts for OSC 777 and needs-you transitions (one per notification, however many windows) | `taskbar_flash_toasts` (with row 17): taskbar state, flashes, toasts shown and present in the Action Center history | ✓ |
| G3 | **File drag and drop.** Dropped files are pasted as quoted paths; in remote mode they are uploaded (`Request::Upload`, 50 MB each, into `<home>/uploads/<date>/`) and the remote paths pasted; a folder dropped on the canvas opens the new-session dialog there | `drop_files_local` (quoted path at the prompt; folder → dialog) and `remote_loopback_gui_and_upload` (row 33). Unit: `files.rs` (pieces, quoting), backend upload (names, order, cap) | ✓ |
| G4 | **Several windows.** Ctrl+Shift+N or the toolbar's ⧉ opens another window on the same canvas: its own `App`, connection and view preferences; the service's document sync keeps them consistent; the process ends with its last window | `second_window_rename_sync`: a rename in window 1 appears in window 2; closing window 2 leaves window 1 | ✓ |
| — | **Pasted screenshots** are shown as a thumbnail with their size for 5 s (the agent attaches the image itself) | row 15 | ✓ |

## Canvas visuals (rows V1–V6)

Both front ends have these features (the TUI draws them as characters and
switches without animation; the ViewModel snapshot tests `dusty-sessions`,
`stacked-subsessions`, `hidden-subsessions`, `subagent-decks`,
`subagent-decks-unfolded` and `effects-reduced-motion`
pin both renderings); the GUI-only parts (texture, transitions, particles,
fixed-size chips) have these checks, on a separate canvas of the sandbox
(folded subagent decks: row 46):

| | Feature | How it is tested | ✓ |
|---|---|---|---|
| V1 | **Dust on unused sessions** (static; age in the box and the sidebar; *Select unused sessions*) | `dust_levels_and_select_unused`: boxes last used 0/5/20/90 days ago have dust 0/1/2/3, "unused 20d" in the status line, the ages in the sidebar; the canvas menu selects exactly the three unused ones | ✓ |
| V2 | **Stacks and hidden subsessions** (animated, end states) | `stack_unstack_and_hide`: the session menu stacks; a frame mid-transition shows fading cards; the end state has one deck (the newest on top, +1) beneath the parent and the document says stacked; a double-click unstacks and the boxes are back at exactly the same places; `H` hides both subsessions (⋯ 2 hidden on the parent) and shows them in place | ✓ |
| V3 | **Effects** (`copilot-canvas effect`, acknowledged by a click that starts nothing, animated only while visible) | `effect_fireworks_ack`: fireworks from the CLI appear with their badge; 26 frames in 2 s while they run (≤ 15 fps); a click on the box clears them here, in the service's state and so everywhere, selects the box and starts no session; a few frames in 2 s afterwards (frames that only move the passive session's loop dot are not counted) | ✓ |
| V4 | **Group chips at any zoom (⟲ passive, ✎ notes, ☑ todos); sidebar folder and passive rows** | `chips_low_zoom_and_sidebar`: the sidebar lists the folder (◧) and the passive session (⟲); at 25 % the title row has fixed-size chips, the ✎ notes chip among them (the group has a folder); ⟲ 1 opens the group's passive panel (“Passive sessions of “Visual group””, the session, canvas-wide agents apart) and its Watcher row the session's panel (no session starts) | ✓ |
| V5 | **Group files in remote mode** (`--remote loopback`) | `remote_group_files_in_canvas`: the group panel's *◧ Open AI docs* opens the canvas's folder browser with the folder's files (the title's ✎ Notes chip, which replaced the ◧ / ◨ folder chips, opens the Notebook: row 48b); a click on `notes.md` shows it in the read-only viewer; the passive panel's *Rules* shows the rules in the viewer; the service log shows no Explorer or program opened for the client | ✓ |
| V6 | **Where a group's passive sessions are, in remote mode** (`--remote loopback`; WI-021) | `remote_group_passive_sessions`: the sidebar's “⟲ Passive sessions 1” row opens the group's passive panel (title with the count, “They belong to this group”, the session with its state and *Run now* / *Pause*, “Canvas-wide agents · 0” apart); the title chip (“⟲ 1 passive”, or compact “⟲ 1”) opens the same panel; *Open* shows the session's panel (with *Group's passive*); no session starts. Row 25 creates its passive session from the empty group's ⟲ + passive chip: the panel's empty state (“No passive sessions in this group yet”) and *+ Create a passive session for this group…*, then the chip reads ⟲ 1 | ✓ |

## Last run

Row 52 (the ✦ Skills view and group skill folders, `parity_skills.rs`) is
new; the suite has 70 rows with it. Verified 2026-11-01: `PASS: 70 rows in
267.8s` (row 52 in 1.7 s: `pa-grp (group menu › Skill folders…, a missing
folder refused) and pa-proj (.claude/skills where the sessions run) listed
for every agent and for GitHub Copilot, OpenCode, Claude Code; nothing
launched`). The shared helper that opens the parity group's menu now
right-clicks a free cell of its title row: in the full run an earlier row
leaves a box over the title's start, and the press opened that box's menu.
The 45-step self-test passed in the TUI, over loopback, in the GUI and in the
GUI over loopback; 11 golden screenshots and nine hardening checks passed.
The agent check's skills step (`selftest agent`, not run here: it needs the
real agents) now waits for the view's *Agent: All agents* row, the canvas
plugin's `canvas-events` and PROJECTS (the view's last rows can be below the
first screen).

Merged with WI-023 (2026-11-01): its dot-grid pulse row, numbered 47 on
`main`, is row 51 here (47 to 50 are the spaces rows). Verified on the
merged tree: `PASS: 69 rows in 268.1s`; the 45-step self-test passed in the
TUI (145.0 s), over loopback (145.0 s), in the GUI (142.3 s) and in the GUI
over loopback (143.2 s); the 11 golden screenshots matched without
re-blessing; nine hardening checks passed; the view snapshot `grid-pulse`
gains the toolbar's space tabs.

The Notebook, Tasks and spaces (docs/NOTEBOOK-TASKS.md) add rows 41d, 47, 48,
48b, 49 and 50 (`parity_spaces.rs`): spaces, Go to and the history; the
Notebook's explorer, filter, reader, editor, quick capture and conflicts; the
same over `--remote loopback`; Tasks on a book written through `copilot-canvas
todo` and the sandbox service (an agent's proposal accepted, a card dragged,
details); the ✎ / ☑ chips and a session's ☑ badge; HTML notes served from the
notes origin with their scripts off, allowed and off again, `#note:` /
`#todo:` links and *Print / PDF*. Rows 9, 37 and V5 follow the ✎ Notes and ☑
chips that replaced the ◧ AI docs / ◨ Outputs chips (row 9 clicks them, row
37's click target is `Notebook(OpenGroup)`, V5 reaches the remote folder
browser through the group panel's *Open AI docs*); V4 also checks the ✎ chip
at 25 %. The shared self-test has 45 steps (six new: switching spaces, quick
capture and editing, the ✎ chip and the AI filter, Go to and the history,
Tasks on the demo book, todos through the CLI on a canvas of its own); the
parity suite has 68 rows.

Product fixes found on the way (each with a unit test): the todo details'
menu button `☰` is two cells wide since Unicode 16 while Windows' console
draws it in one (the TUI's rows shifted), now `≡`; in demo mode the service's
snapshots, which carry no book there, undid the window's own todo changes (an
accepted proposal came back); Ctrl+E from the explorer left the keyboard
there (typing reached the explorer's keys); the first save of an agent's
note without front matter wrote `author: mixed` above the caret while the
caret kept its line number (typing went on inside the front matter), and the
same for *Mark reviewed*.

Verified 2026-10-31: `PASS: 68 rows in 255.1s` (row 48 in 4.8 s: `✎ chip →
the group's folder (● summary.md, ◇ plan.md); Human hid the agents' notes;
Rendered in the HTML view (wiki and todo links); Source, Ctrl+S, Live;
reloaded a clean note, Take theirs on a conflict; Alt+N → Inbox`; row 41d in
2.6 s: `page.html from https://notes.canvas.invalid/ with its style.css and
pixel.png; scripts off (0 kept), on for the file (it ran, sandboxed), off
again; #todo: and #note: links → 2 app links; print dialog without error; 0
connections to the listener`; row 50: `chip "☑ 3 · 1▸ · 1◇" (gold) → after
the decline "☑3"`; V4: `3 chips at 25 % (⟲ 1 ✎ ☑+)`); the 45-step self-test
passed in the TUI (145.1 s), over loopback (145.0 s), in the GUI (143.4 s)
and in the GUI over loopback (142.2 s); nine golden screenshots re-blessed
after review (the toolbar's ◫ / ✎ / ☑ tabs, the groups' ✎ Notes / ☑ chips,
the demo sessions' ☑ WI-002 / ☑ WI-003 badges) and two new ones
(`tasks-board`, `notebook-source`), then all 11 matched pixel-for-pixel; nine
hardening checks passed (the UIA client finds the three toolbar TabItems, the
canvas's selected, and switches to the Notebook through one).

WI-023 makes the canvas's dot grid breathe while an agent works: a slow
3 s brightness pulse of the background dots (about 40 % brighter and a
touch bluer at its top, the normal colour at its bottom), faded in and out
over 0.8 s, from the animation clock; the GUI repaints only the grid's own
layer for it (in frames of its own at most 15 a second, and only when its colour changes), the TUI
steps through three lighter colours on its spinner frames. Reduced motion,
effects off and the new *Settings › Dot grid pulse* off keep it still, and
the `freeze` hook holds the normal colour. Row 51 is new (above); the
shared self-test remains 39 steps; the parity suite has 63 rows.

Verified 2026-10-31: `PASS: 63 rows in 256.2s` (row 51 in 18.1 s: `24
dots: top of a breath +16/+25/+35 (level 0.350 at 1475 ms), bottom ±0
(level 0.000, 1500 ms apart in the breath); while working in 2 s: 22 grid
repaints, 8 full frames (0 and 9 with the pulse off); still with reduced
motion, effects off and the setting off (saved); fade-out seen: true, then
2 frames in 2 s`; two more runs of the row alone: 22 grid repaints, 1 frame
in 2 s once nothing worked); the nine golden screenshots matched pixel for
pixel without re-blessing (their canvases have working sessions, and the
`freeze` hook holds the grid at its normal colour); no view snapshot
changed and `grid-pulse` is new; nine hardening checks passed.

The run before (WI-021) makes a group's passive sessions discoverable: a sidebar row
“⟲ Passive sessions N” (or “Passive: none · + add…”) under every group, the
labelled title chip (⟲ N passive / ⟲ + passive), *Passive sessions of this
group…* in the group menu and a PASSIVE SESSIONS section in the group's
details all open the group's passive panel (state, Open / Run now / Pause,
an empty state with Create, canvas-wide agents apart). Row V6 is new; row 25
creates through the chip's empty state; V4 goes chip → panel → session. The
shared self-test remains 39 steps; the parity suite has 62 rows.

Verified 2026-10-30: `PASS: 62 rows in 252.8s` (V6: `over TCP: sidebar row →
panel (1 session, state, buttons, 0 canvas-wide agents apart); chip → panel;
Open → its panel; nothing started`); two golden screenshots re-blessed after
review (all-statuses and visuals: the sidebar's new ⟲ Passive row and the
“⟲ 1 passive” chip) and then matched; the view snapshots
`group-passives-panel` and `group-passives-empty` are new and ten others gain
the sidebar row; nine hardening checks passed.

The run before (WI-022) folds every session's subagent boxes into one slim deck beneath it by
default. Row 46 is new (above); rows 5, 35, 44 and 45 keep their intent and
now work with folded subagent boxes: row 5 finds “Parity explorer” in its
session's deck, row 35 inspects it through the deck's list and shows the
OpenCode children with `K` first, row 44's group drag carries a folded box,
row 45 shows the parent's subagent box with `K` before selecting it (and `H`
now hides subagent boxes too). Row 35's `inspect` helper waits for the
clicked box's own details panel (the panel before it has the same button).
The shared self-test remains 39 steps (its area-selection step shows the
demo's folded subagent box with `K` first and folds it again); the parity
suite has 61 rows.

Verified 2026-10-30: `PASS: 61 rows in 235.2s` (row 46 in 3.9 s:
`deck “2 subagents” working; a click listed them; K showed them at their
saved places (a transition frame seen: true); still shown after a restart; H
hid them (◇ 2 hidden, ↳ 2 hidden) and showed them; K folded them again;
nothing started`); seven golden screenshots re-blessed after review (the demo
canvas, its 50 % zoom, context menu, new-session dialog, tiled terminals and
reports inbox: the subagent box is now a “◇ 1 subagent” deck, the session's
details list its subagents and its menu offers *Show subagents*; all-statuses:
“With agents”'s subagent is a “1 subagent · working” deck) and then matched
pixel-for-pixel; nine hardening checks passed.

The run before (WI-020): row 1 clicks the discoverable **Show all nodes**
toolbar button after panning and zooming, checking the recovered origin and
zoom cap; `PASS: 60 rows in 227.0s`; the parity driver's clicks avoid
fixed-size group chips at overview zoom and fit an offscreen box before
opening its terminal.

```
row  | feature                                                        | GUI test                                     | ✓/✗
1    | Canvas: drag boxes, pan, zoom, fit, center, double-click crea… | canvas_pan_zoom_menus                        | ✓
2    | Status (working, needs you, done · review, idle, stopped, fai… | status_from_history                          | ✓
3    | Attention borders and tints; active-terminal halo (heavy whil… | attention_and_halo                           | ✓
4    | Colour-blind-safe palette; distinct icon and border per state  | state_icons_and_borders                      | ✓
5    | Subagents from history, folded into a deck beneath their sess… | subagent_boxes_from_history                  | ✓
6    | Subsessions: S, fork or fresh, dotted connector; spawned dete… | subsession_with_dashed_edge                  | ✓
7    | Looping sessions: loopback arc, ↻ every, sidebar ↻, pulsing ↻  | loop_from_session_schedule                   | ✓
7b   | Looping sessions: the dot follows the clock smoothly (loop la… | loop_dot_smooth_and_reduced_motion           | ✓
8    | MCP picker with presets (Ctrl+S, ←/→, Del), per session and r… | mcp_picker_preset_and_launch                 | ✓
9    | Groups: select, G, move by title, resize ◢, nested, membershi… | group_move_resize_chips                      | ✓
10   | Sidebar tree, / filter, click flies to the box, group rows ti… | sidebar_filter_click_and_group               | ✓
11   | Terminals: ConPTY, single or 4 tiles (T), survive client exit  | tiles_single_and_tiled                       | ✓
11b  | Terminals survive client exit (detach, reattach)               | detach_and_reattach                          | ✓
12   | Wheel rules (mouse mode, PgUp/PgDn, scrollback, Shift forces … | wheel_scrollback_and_shift                   | ✓
13   | User-message tint (navy background, ▎ bar) in Copilot boxes    | user_message_tint_png                        | ✓
14   | Copy and paste: drag-select (rejoin), copy on release, Ctrl+C… | copy_on_release_and_paste                    | ✓
15   | Image paste (Alt+V / Ctrl+V): direct Ctrl+V; show the image    | paste_image_preview                          | ✓
16   | Click forwarding in Copilot's mouse mode; drags still select   | click_forwarding_and_drag_select             | ✓
17   | Title in the pane header, taskbar state mirror, bell, OSC 777… | title_progress_bell_toasts                   | ✓
18   | Links: URLs, OSC 8, Windows paths; Ctrl+click; nothing execut… | links_osc8_and_paths                         | ✓
19   | New session dialog: name, folder (Browse…, Recent ▾), Git, wo… | new_session_dialog_fields                    | ✓
20   | Import (filter, multi-select, grouping); VS Code sync (+ Ctrl… | import_dialog_and_vscode_view                | ✓
21   | Activity bar views (Alt+1…9)                                   | activity_views_alt_1_to_9                    | ✓
22   | Copilot defaults, group overrides, copilot config list and ed… | settings_defaults_and_config                 | ✓
23   | Plugins, MCP, skills: list, toggle, remove, add, install; sec… | mcp_view_lists_and_toggles                   | ✓
24   | Migrate from VS Code (preview, tick, backup, redacted log)     | migration_preview_dialog                     | ✓
25   | Passive sessions: templates, panel, dashed ⟲ boxes, ✉, pulses… | passive_create_and_panel                     | ✓
26   | Automations: families, failure pulse, badge, info and health,… | automations_view_and_details                 | ✓
27   | Copilot automations (data.db) and session schedules            | automation_boxes_and_session_loops           | ✓
28   | Idle auto-stop, priority manager, upgrade with resume, protoc… | crash_log_and_build_match                    | ✓
29   | Keyboard map, F6 / Ctrl+] to leave a terminal, Q quit dialog … | help_f6_and_quit_dialog                      | ✓
30   | --demo mode                                                    | demo_mode_canvas                             | ✓
31   | --perf overlay: GPU timings, round-trip time                   | perf_overlay_rtt                             | ✓
32   | CLI (service, doctor, bench, events, emit, send, status, foll… | cli_unchanged (no front end: the same CLI b… | ✓
33   | New: remote mode (pair, reconnect, resync, offline input bloc… | remote_loopback_gui_and_upload               | ✓
34   | New: view versus start (a click views; double-click, Enter, S… | view_vs_start                                | ✓
35   | New: transcript inspector (sessions and subagents, live while… | inspector_and_child_resume                   | ✓
36   | New: removing a multi-selection (its eligible boxes after a c… | multi_remove_keeps_running                   | ✓
37   | New: a click on a group's background or border selects it; ti… | group_background_and_border_select           | ✓
38   | New: RECENT sessions atop the Sessions panel (newest first, s… | recent_sessions_and_walk_keys                | ✓
39   | New: the default model (shown before starting; a choice appli… | remembered_default_model                     | ✓
39b  | The remembered default model after the client restarts         | remembered_model_after_restart               | ✓
40   | New: canvas-wide agents: ◎ Agents (top bar), the Canvas agent… | canvas_agents_one_agent_every_entry          | ✓
40b  | New: a canvas-wide agent runs on an event from any group (sta… | canvas_agent_runs_across_groups              | ✓
41   | New: Reports inbox: sessions publish reports and messages (co… | reports_inbox_and_viewer                     | ✓
41b  | New: HTML reports in the GUI's WebView2 view: their own style… | html_report_locked_down (GUI only: the TUI … | ✓
41c  | New: remote mode lists and opens reports through the service … | remote_reports_inbox                         | ✓
41d  | New: HTML notes in the GUI's WebView2 view from the notes ori… | html_note_pane (GUI only: the TUI shows HTM… | ✓
42   | New: PR coordination: a session hands a PR off; the canvas po… | pr_handoff_follow_up_and_end                 | ✓
43   | New: review follow-ups: a review is handed off; the agent's d… | review_handoff_message_and_request           | ✓
44   | New: Ctrl+Z undoes a group drag (boxes, subagents, nested gro… | layout_undo_redo                             | ✓
45   | New: Shift-drag selects subagent boxes and subsessions like s… | subsession_area_selection                    | ✓
46   | New: subagent boxes fold into a deck beneath their session by… | subagents_folded_by_default                  | ✓
47   | New: spaces: toolbar tabs and Ctrl+1/2/3; Go to (Ctrl+P) over… | spaces_goto_and_history                      | ✓
48   | New: the Notebook: explorer (a group's folder, ● / ◇), Human … | notebook_explorer_reader_editor              | ✓
48b  | New: the Notebook and Tasks in remote mode: listing, reading … | remote_notebook_and_tasks                    | ✓
49   | New: Tasks: the todo canvas (boards, columns, cards) of a boo… | tasks_canvas_details_accept_drag             | ✓
50   | New: canvas links: a group's ✎ Notes and ☑ chips (counts, gol… | chips_and_badges_navigate                    | ✓
51   | New: the dot grid breathes gently while an agent works (fades… | grid_pulse_while_working                     | ✓
5
G1   | New: inline images (Kitty graphics; iTerm2 OSC 1337 through C… | kitty_image_pixels_png (GUI only: the TUI c… | ✓
G2   | New: taskbar progress (ITaskbarList3), FlashWindowEx, toasts   | taskbar_flash_toasts (GUI only: a console h… | ✓
G3   | New: file drag and drop (paths pasted; uploaded in remote mod… | drop_files_local (GUI only: a console canno… | ✓
G4   | New: several windows on one canvas (Ctrl+Shift+N, toolbar)     | second_window_rename_sync (GUI only: a cons… | ✓
V1   | Dust: unused sessions dusty by age (static), age in the box a… | dust_levels_and_select_unused (the TUI dims… | ✓
V2   | Stacks: K or the menu stacks subsessions (animated), double-c… | stack_unstack_and_hide (the TUI switches wi… | ✓
V3   | Effects: copilot-canvas effect on a box, animated ≤ 15 fps wh… | effect_fireworks_ack (the TUI shows a badge… | ✓
V4   | Group chips at any zoom (⟲ passive, ✎ notes, ☑ todos), sideba… | chips_low_zoom_and_sidebar (the TUI draws t… | ✓
V5   | Remote mode: group folders in the canvas's browser, passive f… | remote_group_files_in_canvas (the TUI uses … | ✓
V6   | Remote mode: a group's ⟲ Passive sessions row and chip open i… | remote_group_passive_sessions (the TUI draw… | ✓
PASS: 70 rows in 267.8s
```

Rows 42 and 43 in the WI-020 run: `tracked by its owner; baseline poll; 3 comments
→ 1 follow-up, queued while typed into, delivered when idle; Reports
["queued", "delivered", "ok"]; merged → no more polls (44 gh calls, none a
write)` (53.1 s); `handed off with notes; draft awaited approval; approved →
sent once to outbox:ana (no notes); re-review read on the check, recorded
once, delivered to the reviewer; Reports ["warn", "ok", "queued",
"delivered"]` (56.8 s). Both leave the parity group in view and every report
read, as the rows after them expect.

Rows 41–41c in the run before: `2 published as Stopped one, ✉2 → ✉1 → none,
toasts ["quiet", "quiet"], read at the service, nothing launched`;
`WebView2 ready: styles applied (5104 magenta px), composed over the content
area (5104 px), 0 scripts kept, raw script not run, 2 request(s) answered 403
and 0 reached the listener, navigation blocked`; `listed (✉1) and opened over
loopback TCP; read at the service`. Rows 44 and 45 (2026-10-29): `group drag
(6 boxes, nested group) by (8, -2) and ◢ resize undone one at a time to the
exact places in the canvas file, redone and undone again; Ctrl+Z in the shell
reached the paste probe`; `100 %: parent + subagent, + subsession, each moved
once by (3, 1); 50 %: parent + subagent, + subsession, each moved once by
(6, 2); hidden ones left out (4 selected); nothing started`.

## Limits of the checks

- Toasts are verified as shown by Windows and present in the Action Center
  history; whether they pop up depends on Focus Assist. When the window is
  in front the GUI deliberately shows none (the check accepts that).
- With a locked workstation the clipboard rows use the hooks' clipboard
  (see above); the Windows clipboard itself is covered by canvas-core's
  clipboard test on an unlocked desktop.
- Copilot's own UI is replaced by the stand-in; the real-agent behaviours
  stay with `selftest agent` and the manual probes in `src/selftest.rs`.
- Rows 42 and 43 never touch a real pull request or person: `gh` and `az`
  are stand-ins (`COPILOT_CANVAS_GH` / `COPILOT_CANVAS_AZ`: `copilot-canvas.exe`
  copies answering from fixtures in the sandbox and refusing any write, which
  the rows check), the messaging channel is a folder in the sandbox, and the
  agents are the stand-in following the skills' contracts. Checks run every
  2 s (`COPILOT_CANVAS_PR_POLL_SECS`) and review checks every 10 s
  (`COPILOT_CANVAS_REVIEW_CHECK_SECS`) instead of 5 and 60 minutes.
- Row 41d checks that *Print / PDF* opened WebView2's print dialog without
  an error, not what it would print; the offscreen captures (`png`) never
  contain the WebView2 view, so the Notebook's golden screenshot shows a note
  in Source (cells) and the HTML view is checked through the `webview` hook.
- Row 51 compares the grid's dots in offscreen PNG captures with a
  reduced-motion capture of the same view (its frame counts leave out the
  loop frames of the sandbox's looping automations); how a given monitor
  shows such a small change is a matter for the eye, not the suite. The
  TUI's colour steps are checked by canvas-core unit tests (the view
  snapshots hold characters, not colours).
- The checks assert behaviour and a few pixels, not whole images. Visual
  regressions are the job of `selftest --client gui --golden`
  (`docs/GUI.md` §3), whose `all-statuses` golden screenshot is this suite's
  sandbox canvas (every status, a subagent folded into its session's deck, loops, the automations group),
  and of canvas-core's ViewModel snapshot tests, which pin the same states
  as text for both render modes.
