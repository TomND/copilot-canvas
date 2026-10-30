# GUI parity (PLAN §5 Phase 5, §8)

Every row of the parity matrix in `PLAN.md` §8 works in the GPU client and
has an automated GUI check. The checks run in one command:

```powershell
cargo build --release
target\release\copilot-canvas.exe selftest --client gui --parity
```

It takes about 110 s on the development machine (row 40b waits out event debounces; RTX 3070, workstation
locked), prints each check as it runs and ends with the table below;
`CANVAS_GUI_SCREENS=DIR` keeps the PNG captures the checks inspect.
`CANVAS_PARITY_ROWS=41,41b` (a comma-separated list of row numbers) runs only
those rows, for work on one feature; rows outside the list are not run and
not listed, and the full suite is what counts.

## How the suite works

- **Over the test hooks.** It starts `copilot-canvas-gui.exe --test-hooks
  --size 170x46 --perf` and drives it like the 36-step self-test
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

`GUI check` is the parity suite's check; `also` lists the 36-step GUI
self-test steps (`selftest --client gui`, also run with `--remote loopback`)
and unit tests that cover the row.

| # | Feature (§8) | GUI check | Also | ✓ |
|---|---|---|---|---|
| 1 | Canvas: drag boxes (subagents follow), pan, zoom, fit, center, double-click creates, right-click menus | `canvas_pan_zoom_menus`: middle-drag pans, Ctrl+wheel zooms in, `F` fits, a box drag moves it in the document, right-click menu, double-click opens the new-session dialog | steps "mouse drag moves a session together with its subagent", "right-click opens a context menu; toolbar zoom buttons"; canvas-core model tests (pixel hit testing, continuous zoom) | ✓ |
| 2 | Status (working, needs you, done · review, idle, stopped, failed, open elsewhere), OSC 9;4, background subagents, Esc not counted as done | `status_from_history`: all seven states plus a background subagent from synthetic histories; an `abort` turns working into idle, not done · review | step "progress (OSC 9;4), title and bell reach the canvas"; `status.rs` tests | ✓ |
| 3 | Attention borders and tints; active-terminal halo (heavy while typing) | `attention_and_halo`: needs you and review boxes carry attention colours; the halo is heavy while typing | step "the active terminal's box gets a halo" | ✓ |
| 4 | Colour-blind-safe palette; distinct icon and border per state | `state_icons_and_borders`: needs you, review and idle have three different borders; ◆ ✓ ○ ■ ✗ ↗ all drawn | canvas-gui `theme.rs` palette test | ✓ |
| 5 | Subagents from history | `subagent_boxes_from_history`: “Parity explorer” box with its edge from `subagent.started` | | ✓ |
| 6 | Subsessions: `S`, fork or fresh, dotted connector; spawned-subsession detection | `subsession_with_dashed_edge`: `S` → dialog → linked child, connector added | steps "subsession linked to its parent", "a Copilot session started inside a session's terminal appears as its linked subsession" | ✓ |
| 7 | Looping sessions: loopback arc with a moving dot, `↻ every`, sidebar ↻, pulsing ↻ | `loop_from_session_schedule`: `↻ every 15m`, an arc, sidebar ↻ | step "repeating automations get the animated loopback" (the dot moves between frames) | ✓ |
| 7b | **New:** a smooth, lively loop dot from the clock; reduced motion keeps a still indicator | `loop_dot_smooth_and_reduced_motion`: five PNG captures 90 ms apart each show the lit dot within 3 px of where the clock put it (the hooks report it per capture), the lap advances with the time between captures and the dot moves a little every time, never jumps; the loop layer redrawn at its pace while in view (its own loop frames plus the full frames other animations need: at least 65 % of 2 s / its interval), no loop frames once every loop is scrolled away; with reduced motion no dot, two captures of the arc identical, no loop frames, `↻ every 15m` kept | canvas-core `loops` tests (continuity across frames and laps, frame-rate independence, cell stepping, grow-in and fade-out, TUI redraws only for visible dots), canvas-gui `the_loop_dot_glides_along_the_curve_at_any_frame_rate`; snapshots `looping-sessions`, `looping-sessions-reduced-motion`; `bench --client gui --loops` | ✓ |
| 8 | MCP picker with presets (Ctrl+S, ←/→, Del), per session and resume | `mcp_picker_preset_and_launch`: servers from the CLI, none selected, Ctrl+S saves preset “Parity none”, the launch passes `--disable-mcp-server` twice (the stand-in prints them), preset and per-session list saved | | ✓ |
| 9 | Groups: select, `G`, move by title, resize ◢, nested, geometric membership, folder chips, launch defaults | `group_move_resize_chips`: folder chips, ◢ resize, move by title with its members | steps "right-drag rubber-band selection, then G groups", "a new group asks for its folder" | ✓ |
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
| 25 | Passive sessions: templates, panel, dashed ⟲ boxes, ✉ badge, pulses, idle-only messages, mute, rollover, budgets | `passive_create_and_panel`: group menu → template → name → panel, rules scaffolded, a dashed ⟲ box | canvas-core passive tests | ✓ |
| 26 | Automations: families, failure pulse, toolbar badge, info and health page, run, enable, ask doctor, outputs | `automations_view_and_details`: listed with prompt, schedule and runs | steps "Automations view lists Copilot automations", "clicking an automation … Space toggles it" | ✓ |
| 27 | Copilot automations (`data.db`) and session schedules | `automation_boxes_and_session_loops`: three automation boxes; the session schedule loops | | ✓ |
| 28 | Idle auto-stop, priority manager, upgrade with resume, protocol-mismatch tolerance, crash log | `crash_log_and_build_match`: no update prompt for a current service; a crash is written to `crash.log` and the sessions keep running (idle stop, priorities and upgrades are the service's, shared with the TUI) | `upgrade_probe` (manual), canvas-host tests | ✓ |
| 29 | Keyboard map, F6 / Ctrl+] to leave a terminal, Q / Ctrl+Q quit dialog, `?` help | `help_f6_and_quit_dialog` | steps "Ctrl+] returns keyboard focus", "detach", "stop all and quit" | ✓ |
| 30 | `--demo` mode | `demo_mode_canvas` | the whole 36-step self-test runs in `--demo` (its canvas-wide agents step opens a canvas of its own) | ✓ |
| 31 | `--perf` overlay (GPU timings, round-trip time) | `perf_overlay_rtt`: frame timings and service round trips measured | `bench --client gui` | ✓ |
| 32 | CLI (service, doctor, bench, events, emit, send, status, followup, automations) | `cli_unchanged` — **no front end:** the same CLI binary for both clients | existing CLI tests | ✓ |
| 33 | **New:** remote mode (pair, reconnect, resync, offline input blocking) | `remote_loopback_gui_and_upload`: the GUI over loopback TCP; a dropped file is uploaded to the service's machine and its path pasted | `selftest --client gui --remote loopback` (35 steps over TCP); `selftest --remote faults` (the reconnect core both clients share) | ✓ |
| 34 | **New:** view versus start: a click (canvas, sidebar) only views a box (its live terminal or its details panel); double-click, Enter, the panel's Start / Resume and menu items start it; group rows never launch | `view_vs_start`: a click on a stopped session shows its details with Resume and launches nothing (service log, no tile); a double-click, Enter and the Resume button each resume one (the stand-in Copilot); a group row shows running terminals and launches nothing | steps "a click views a stopped session without launching it", "a double-click on the box starts it", "the details panel's Start button starts a session"; canvas-core `interact` tests | ✓ |
| 35 | **New:** transcript inspector for session and subagent boxes (live while they run); a finished child resumes as its own linked subsession, never beside a writer | `inspector_and_child_resume`: a Copilot session's transcript and a subagent's note; an OpenCode child (synthetic database) that works, whose inspector shows a new line written while it is open; with its parent running (stand-in OpenCode) a finished child offers no resume; parent stopped: **Resume as subsession** adds a linked box and the service resumes it with `-s <child>` | canvas-agents `transcript` tests (Copilot with subagent events, OpenCode with a child, Claude Code with a subagent file), canvas-core `inspect` tests (resume rules, a refused resume), `backend_contract` (local and remote answers match); `selftest agent --agent opencode --prompt` (inspect and resume a real child) | ✓ |
| 36 | **New:** removing a multi-selection: right-click on a selected box (or Del) removes its eligible boxes after a confirmation; sessions running in a canvas terminal stay; history is never deleted; Ctrl+Z restores them | `multi_remove_keeps_running`: a 3-box selection (one running) → menu “Remove 2 from canvas” → the confirmation names the running one → two boxes gone, their history files kept → Ctrl+Z brings them back | canvas-core `remove` tests (eligibility, subagents go with their session, subsessions unlinked, undo) | ✓ |
| 37 | **New:** a click on a group's empty background or its border selects the group (innermost; a few pixels outside the outline in the GUI); title drag, ◢ resize, folder chips, background pan and double-click are unchanged | `group_background_and_border_select`: an empty background spot and a point 0.2 cells outside the outline each select the group (its panel, nothing launched); a background drag pans; title drag and ◢ still move and resize; chips stay clickable; a double-click opens the new-session dialog | step "a click on a group's border selects the group" (heavy outline, its panel); canvas-core `interact` tests (both render modes, nested groups, gestures) | ✓ |
| 38 | **New:** recent sessions: a RECENT section atop the Sessions panel (newest first, shared by every window through the document); Ctrl+F6 / Ctrl+Shift+F6 (terminals too) and `]` / `[` view the next or previous one and never start it | `recent_sessions_and_walk_keys`: three sessions viewed in turn are listed newest first and saved in the canvas document; Ctrl+F6, Ctrl+F6, Ctrl+Shift+F6 select the one before, the one before that and back (details panels, a "Recent 2 of N" notice) while the list keeps its order; a second later the one reached is first; `]` and `[` on the canvas do the same; no launch in the service log, no terminal for any of them | step "recent sessions: Ctrl+F6 / Ctrl+Shift+F6 view recent sessions without starting them" (from a terminal, TUI and GUI); canvas-model `recent` tests (order, clock skew, walk, removal, document round trip and two windows merging), canvas-core `recent` tests (keys never launch, the walk settles after a second or on any other input, Ctrl+F6 in a terminal, `]` reaches the agent there, the RECENT rows hold still for a double-click); snapshot `sidebar-recent` | ✓ |
| 39 | **New:** the default model for new sessions: the new-session dialog shows the model it starts with and why (group override, remembered default, initial Claude Opus 5.5); a pick applies to that session and becomes its agent's remembered default (in a group with its own model, that session only); Settings shows and resets each agent's default; resumes keep a session's own model; 39b: still remembered after a restart | `remembered_default_model`, `remembered_model_after_restart`: the stand-in Claude Code starts with `claude-opus-5-5` (nothing chosen, nothing remembered); a model picked from the list starts the next session and is remembered and recorded on its box; a resumed older session keeps `--model claude-opus-5-5`; a Copilot resume carries no model; a group override is shown "from group", a pick there is for that session only, then reset; a restarted GUI still shows the remembered model | `selftest agent` and `selftest agent --agent claude` (the launch command line carries the default model); canvas-model `models` tests (resolution, migration of the old single model, round trip), canvas-agents launch tests (new versus resume per agent); snapshots `new-session-dialog`, group model and model picker | ✓ |
| 41 | **New:** the Reports inbox: sessions publish reports and short messages (`copilot-canvas report publish`); an unread count on the activity bar, a notice and a toast; the viewer formats Markdown and marks it (its thread) read at the service; viewing its session starts nothing | `reports_inbox_and_viewer`: a stopped session's environment publishes a Markdown brief (`--kind warn --pr`) and a queued message in a thread; ✉2 on the activity bar, "New report from Stopped one", a toast per report asked for (quiet while the window is in front); a click on the brief shows it formatted (bullets, a table with rules, a quote), ✉1, read in the service's `state.json`, the message still unread; *View “Stopped one”* closes the viewer and shows its details panel (Resume), the message reads with one click, nothing unread; the service log shows no launch | step "reports: a session publishes a Markdown report and a message …" (TUI and GUI, also over loopback); canvas-core `reports` tests (threads, read on open, never a launch, unread/delete/links through the service), canvas-agents `reports` tests (checks, queued then stored, refusals, retention), `backend_contract` (local and remote inbox, push only to `CAP_REPORTS` clients); snapshots `reports-inbox`, `report-viewer-markdown`, `report-viewer-html`; goldens `reports-inbox`, `report-viewer` | ✓ |
| 41b | **New, GUI only:** HTML reports in a WebView2 view with their own styles; scripts never run, nothing is fetched, navigation blocked (the TUI shows HTML as formatted text) | `html_report_locked_down`: an HTML report with a `<style>`, a `<script>`, an `onerror`, an external stylesheet, CSS and image URLs pointing at a local listener; the view shows it (the viewer says "HTML view: scripts off, nothing loaded from the network"); in the page: no script element, the image without `src`, the CSP meta, the heading magenta from the page's own CSS, the title unchanged; the view's capture has the magenta heading and the composed window (`PrintWindow`) has it only inside the viewer's content area; then an unsanitized page (`probe_raw`): its script element is there but does not run, its requests are answered 403, a `location.href` change is cancelled and the page stays; the listener got no connection; Esc hides the view | canvas-core `report_html` tests (sanitizer, CSP document), `richtext` tests (HTML as text) | ✓ |
| 41c | **New:** remote mode lists and opens reports through the service | `remote_reports_inbox`: a message published on the service's machine; a GUI over `--remote loopback` shows ✉1, opens it (`Request::ReportContent`) and it is read in the service's `state.json` | `selftest --client gui --remote loopback` (the reports step over TCP); `backend_contract` over loopback TCP | ✓ |
| 44 | **New:** undo canvas movement: Ctrl+Z takes back this window's latest layout change, one completed gesture per step (a drag of boxes, a stack or a group with everything that moved with it, a ◢ resize, *Fit to members*, a burst of arrow keys), to the exact places and sizes; successive undos go further back through moves, removals and syncs in one history; Ctrl+Y / Ctrl+Shift+Z redo; nothing starts or stops; a terminal with the keyboard gets Ctrl+Z itself | `layout_undo_redo`, on a canvas of its own (a group with a parent and its subagent box, a nested group with a subsession and its subagent box, a shell, another box): the group dragged by its title (every box and the nested group moved by the same delta) and resized by ◢; in the shell's terminal the paste probe receives Ctrl+Z (`KEY \x1a`) and the canvas file keeps both changes; on the canvas Ctrl+Z restores the size, a second Ctrl+Z every place exactly (the canvas file), Ctrl+Y moves it again and Ctrl+Z back; no launch in the service log | step "Ctrl+Z undoes a group drag exactly … in a terminal Ctrl+Z reaches the program" (TUI and GUI, also over `--remote loopback`); canvas-core `undo` tests (box, group with members, children and nested groups, resize, multi-selection moved once, arrow-key bursts, removals and syncs in one history, the 100-step limit, items removed or moved by another window since, the undo as a document edit, Ctrl+Z in a terminal, a dialog and the sidebar filter, *Fit to members*, stacks), canvas-model `a_selection_with_its_children_moves_each_box_once` | ✓ |

| 40 | **New:** canvas-wide agents: the top bar's **◎ Agents** (a management panel: each agent's state and controls, the canvas skills, the group passive sessions for contrast), the *Canvas agents* frame (◎ chips) and the sidebar show one agent per box; templates (PR coordinator, Cross-group digest, Custom); canvas skills given per agent; viewing one never starts it | `canvas_agents_one_agent_every_entry`: ◎ Agents → *+ New canvas agent* → *PR coordinator* → named: a dashed ◎ box in the new frame, its files under `<home>/canvas-agents/<canvas id>/agents/<slug>`, routed as canvas-wide for this canvas id; the panel's row, a click on the box and the sidebar row select the same box (its panel); a double-click launches nothing; a skill of the canvas library given with *Skills…* (document, routing, panel) | step "canvas-wide agents: ◎ Agents opens their panel; a new agent is one box in the Canvas agents frame …" (TUI and GUI, and over `--remote loopback`); canvas-core `canvas_wide` tests (state machine, routing per canvas id, disable, pause, stop, frame, group filter), canvas-model `canvas_wide` and `bus` tests (folders per home and canvas, skills, plugin, prompt, subscribers across groups, isolation, permissions), canvas-agents backend, CLI and provider tests; snapshots `canvas-agents-panel`, `canvas-agent-detail` | ✓ |
| 40b | **New:** a canvas-wide agent runs on events of every group of its canvas (never another canvas's), with its skills delivered to the agent CLI; Pause, Run now, Stop run, Disable | `canvas_agent_runs_across_groups`: a `pr.published` from a session of the parity group runs the agent with the stand-in Copilot (`-p`, its skills plugin as a second `--plugin-dir`), which reports `fake run of pr.published · skills: parity-skill` through `copilot-canvas status`; paused, an event of the canvas and one from a session no canvas routes start nothing (12 s); *Run now* then *Stop run* ends the run as stopped with no failure; disabled, it is no longer routed and an event starts nothing | canvas-agents `passive` tests (addressed events, effects on any box for canvas-wide agents, run folders, state kept for another canvas), canvas-model `bus` tests | ✓ |

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

## Canvas visuals (rows V1–V5)

Both front ends have these features (the TUI draws them as characters and
switches without animation; the ViewModel snapshot tests `dusty-sessions`,
`stacked-subsessions`, `hidden-subsessions` and `effects-reduced-motion`
pin both renderings); the GUI-only parts (texture, transitions, particles,
fixed-size chips) have these checks, on a separate canvas of the sandbox:

| | Feature | How it is tested | ✓ |
|---|---|---|---|
| V1 | **Dust on unused sessions** (static; age in the box and the sidebar; *Select unused sessions*) | `dust_levels_and_select_unused`: boxes last used 0/5/20/90 days ago have dust 0/1/2/3, "unused 20d" in the status line, the ages in the sidebar; the canvas menu selects exactly the three unused ones | ✓ |
| V2 | **Stacks and hidden subsessions** (animated, end states) | `stack_unstack_and_hide`: the session menu stacks; a frame mid-transition shows fading cards; the end state has one deck (the newest on top, +1) beneath the parent and the document says stacked; a double-click unstacks and the boxes are back at exactly the same places; `H` hides both (⋯ 2 hidden on the parent) and shows them in place | ✓ |
| V3 | **Effects** (`copilot-canvas effect`, acknowledged by a click that starts nothing, animated only while visible) | `effect_fireworks_ack`: fireworks from the CLI appear with their badge; 26 frames in 2 s while they run (≤ 15 fps); a click on the box clears them here, in the service's state and so everywhere, selects the box and starts no session; a few frames in 2 s afterwards (frames that only move the passive session's loop dot are not counted) | ✓ |
| V4 | **Group chips at any zoom; sidebar folder and passive rows** | `chips_low_zoom_and_sidebar`: the sidebar lists the folder (◧) and the passive session (⟲); at 25 % the title row has three fixed-size chips; ⟲ 1 opens the passive sessions menu and its item the panel (no session starts) | ✓ |
| V5 | **Group files in remote mode** (`--remote loopback`) | `remote_group_files_in_canvas`: the ◧ AI docs chip opens the canvas's folder browser with the folder's files; a click on `notes.md` shows it in the read-only viewer; the passive panel's *Rules* shows the rules in the viewer; the service log shows no Explorer or program opened for the client | ✓ |

## Last run

```
row  | feature                                                        | GUI test                                     | ✓/✗
1    | Canvas: drag boxes, pan, zoom, fit, center, double-click crea… | canvas_pan_zoom_menus                        | ✓
2    | Status (working, needs you, done · review, idle, stopped, fai… | status_from_history                          | ✓
3    | Attention borders and tints; active-terminal halo (heavy whil… | attention_and_halo                           | ✓
4    | Colour-blind-safe palette; distinct icon and border per state  | state_icons_and_borders                      | ✓
5    | Subagents from history                                         | subagent_boxes_from_history                  | ✓
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
G1   | New: inline images (Kitty graphics; iTerm2 OSC 1337 through C… | kitty_image_pixels_png (GUI only: the TUI c… | ✓
G2   | New: taskbar progress (ITaskbarList3), FlashWindowEx, toasts   | taskbar_flash_toasts (GUI only: a console h… | ✓
G3   | New: file drag and drop (paths pasted; uploaded in remote mod… | drop_files_local (GUI only: a console canno… | ✓
G4   | New: several windows on one canvas (Ctrl+Shift+N, toolbar)     | second_window_rename_sync (GUI only: a cons… | ✓
V1   | Dust: unused sessions dusty by age (static), age in the box a… | dust_levels_and_select_unused (the TUI dims… | ✓
V2   | Stacks: K or the menu stacks subsessions (animated), double-c… | stack_unstack_and_hide (the TUI switches wi… | ✓
V3   | Effects: copilot-canvas effect on a box, animated ≤ 15 fps wh… | effect_fireworks_ack (the TUI shows a badge… | ✓
V4   | Group chips at any zoom (folders, ⟲ passive), sidebar folder … | chips_low_zoom_and_sidebar (the TUI draws t… | ✓
V5   | Remote mode: group folders in the canvas's browser, passive f… | remote_group_files_in_canvas (the TUI uses … | ✓
PASS: 56 rows in 108.8s
```

Rows 41–41c in that run: `2 published as Stopped one, ✉2 → ✉1 → none,
toasts ["quiet", "quiet"], read at the service, nothing launched`;
`WebView2 ready: styles applied (5104 magenta px), composed over the content
area (5104 px), 0 scripts kept, raw script not run, 2 request(s) answered 403
and 0 reached the listener, navigation blocked`; `listed (✉1) and opened over
loopback TCP; read at the service`.

## Limits of the checks

- Toasts are verified as shown by Windows and present in the Action Center
  history; whether they pop up depends on Focus Assist. When the window is
  in front the GUI deliberately shows none (the check accepts that).
- With a locked workstation the clipboard rows use the hooks' clipboard
  (see above); the Windows clipboard itself is covered by canvas-core's
  clipboard test on an unlocked desktop.
- Copilot's own UI is replaced by the stand-in; the real-agent behaviours
  stay with `selftest agent` and the manual probes in `src/selftest.rs`.
- The checks assert behaviour and a few pixels, not whole images. Visual
  regressions are the job of `selftest --client gui --golden`
  (`docs/GUI.md` §3), whose `all-statuses` golden screenshot is this suite's
  sandbox canvas (every status, a subagent, loops, the automations group),
  and of canvas-core's ViewModel snapshot tests, which pin the same states
  as text for both render modes.
