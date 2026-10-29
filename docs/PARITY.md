# GUI parity (PLAN §5 Phase 5, §8)

Every row of the parity matrix in `PLAN.md` §8 works in the GPU client and
has an automated GUI check. The checks run in one command:

```powershell
cargo build --release
target\release\copilot-canvas.exe selftest --client gui --parity
```

It takes about 25 s on the development machine (RTX 3070, workstation
locked), prints each check as it runs and ends with the table below;
`CANVAS_GUI_SCREENS=DIR` keeps the PNG captures the checks inspect.

## How the suite works

- **Over the test hooks.** It starts `copilot-canvas-gui.exe --test-hooks
  --size 170x46 --perf` and drives it like the 27-step self-test
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

`GUI check` is the parity suite's check; `also` lists the 27-step GUI
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
| 8 | MCP picker with presets (Ctrl+S, ←/→, Del), per session and resume | `mcp_picker_preset_and_launch`: servers from the CLI, none selected, Ctrl+S saves preset “Parity none”, the launch passes `--disable-mcp-server` twice (the stand-in prints them), preset and per-session list saved | | ✓ |
| 9 | Groups: select, `G`, move by title, resize ◢, nested, geometric membership, folder chips, launch defaults | `group_move_resize_chips`: folder chips, ◢ resize, move by title with its members | steps "right-drag rubber-band selection, then G groups", "a new group asks for its folder" | ✓ |
| 10 | Sidebar tree, `/` filter, click flies to the box, group rows tile terminals | `sidebar_filter_click_and_group`: `/Loop` filters, a sidebar click selects the box and moves the canvas to it | step "sidebar click reveals and views a session" | ✓ |
| 11 | Terminals: ConPTY, single or 4 tiles (`T`), survive client exit | `tiles_single_and_tiled` (`T` toggles), `detach_and_reattach` (a new GUI shows the shell's earlier output, then detaches cleanly) | steps "Enter opens a real native terminal", "tiled view", "detach", "reattach" | ✓ |
| 12 | Wheel rules (mouse mode, PgUp/PgDn when mouse off, scrollback, Shift forces scrollback) | `wheel_scrollback_and_shift`: 120 lines, the wheel scrolls back, typing returns to the live screen | canvas-gui `input.rs` wheel notch tests; canvas-host wheel/click encoding tables | ✓ |
| 13 | User-message tint (navy background, ▎ bar) | `user_message_tint_png`: the stand-in Copilot's message row is navy (30, 44, 70) in the PNG, the reply row is not | canvas-termview `user_message_rows` test | ✓ |
| 14 | Copy and paste: drag-select (soft-wrap rejoin), copy on release, Ctrl+C with selection, Ctrl+Shift+C, Ctrl+V, pane menu | `copy_on_release_and_paste`: a drag copies `COPY_PROBE_TEXT` on release; Ctrl+V pastes | canvas-termview selection tests (rejoin) | ✓ |
| 15 | Image paste (Alt+V / Ctrl+V): direct Ctrl+V; show the image | `paste_image_preview`: with an image on the clipboard Ctrl+V goes to the agent and a 64 × 40 thumbnail appears (pixel checked) | canvas-gui `clipimage.rs` (DIB decoding), `images.rs` (thumbnails) | ✓ |
| 16 | Click forwarding in Copilot's mouse mode; drags still select | `click_forwarding_and_drag_select`: the stand-in receives `ESC[<0;7;…M`; a drag selects and copies | | ✓ |
| 17 | Title in the pane header, taskbar state mirror, bell, OSC 777 | `title_progress_bell_toasts`: OSC 0 title in the header; taskbar paused (yellow) while one needs you, busy once answered, paused again; bell counted; OSC 777 and a needs-you transition become toasts (in the Action Center) | step "progress (OSC 9;4), title and bell"; canvas-gui `platform.rs` tests | ✓ |
| 18 | Links: URLs, OSC 8, Windows paths; Ctrl+click; nothing executed | `links_osc8_and_paths`: OSC 8 and plain-path links found; Ctrl+click on a missing path gives a notice | step "links in terminals: plain click focuses, Ctrl+click opens"; canvas-gui tiles link tests | ✓ |
| 19 | New session dialog: name, folder (type, Browse…, Recent ▾), Git detection, worktree, first prompt, MCP row, agent | `new_session_dialog_fields`: every field, the Git probe, the folder browser | steps "new session dialog", "folder browser + Git detection + new worktree session" | ✓ |
| 20 | Import (filter, multi-select, grouping); VS Code sync (+ Ctrl+Z, auto sync) | `import_dialog_and_vscode_view`: filter, Ctrl+A, import two sessions; the VS Code view | step "VS Code view offers sync and migration" | ✓ |
| 21 | Activity bar views (Alt+1…8) | `activity_views_alt_1_to_8`: all eight titles | steps for Automations, Settings and VS Code views | ✓ |
| 22 | Copilot defaults, group overrides, `copilot config` list and edit | `settings_defaults_and_config`: `config --list` shown, the default model edited and saved | step "Settings view edits Copilot launch defaults" | ✓ |
| 23 | Plugins, MCP, skills: list, toggle, remove (confirmed), add, install in a terminal; secrets hidden | `mcp_view_lists_and_toggles`: servers listed from the CLI, Space toggles through it, the plugins view loads | canvas-agents `mcp_listing_shapes_are_normalized_without_secrets` | ✓ |
| 24 | Migrate from VS Code (preview, tick, backup, redacted log) | `migration_preview_dialog`: the read-only preview opens and closes | canvas-agents migrate tests | ✓ |
| 25 | Passive sessions: templates, panel, dashed ⟲ boxes, ✉ badge, pulses, idle-only messages, mute, rollover, budgets | `passive_create_and_panel`: group menu → template → name → panel, rules scaffolded, a dashed ⟲ box | canvas-core passive tests | ✓ |
| 26 | Automations: families, failure pulse, toolbar badge, info and health page, run, enable, ask doctor, outputs | `automations_view_and_details`: listed with prompt, schedule and runs | steps "Automations view lists Copilot automations", "clicking an automation … Space toggles it" | ✓ |
| 27 | Copilot automations (`data.db`) and session schedules | `automation_boxes_and_session_loops`: three automation boxes; the session schedule loops | | ✓ |
| 28 | Idle auto-stop, priority manager, upgrade with resume, protocol-mismatch tolerance, crash log | `crash_log_and_build_match`: no update prompt for a current service; a crash is written to `crash.log` and the sessions keep running (idle stop, priorities and upgrades are the service's, shared with the TUI) | `upgrade_probe` (manual), canvas-host tests | ✓ |
| 29 | Keyboard map, F6 / Ctrl+] to leave a terminal, Q / Ctrl+Q quit dialog, `?` help | `help_f6_and_quit_dialog` | steps "Ctrl+] returns keyboard focus", "detach", "stop all and quit" | ✓ |
| 30 | `--demo` mode | `demo_mode_canvas` | the whole 27-step self-test runs in `--demo` | ✓ |
| 31 | `--perf` overlay (GPU timings, round-trip time) | `perf_overlay_rtt`: frame timings and service round trips measured | `bench --client gui` | ✓ |
| 32 | CLI (service, doctor, bench, events, emit, send, status, followup, automations) | `cli_unchanged` — **no front end:** the same CLI binary for both clients | existing CLI tests | ✓ |
| 33 | **New:** remote mode (pair, reconnect, resync, offline input blocking) | `remote_loopback_gui_and_upload`: the GUI over loopback TCP; a dropped file is uploaded to the service's machine and its path pasted | `selftest --client gui --remote loopback` (27 steps over TCP); `selftest --remote faults` (the reconnect core both clients share) | ✓ |
| 34 | **New:** view versus start: a click (canvas, sidebar) only views a box (its live terminal or its details panel); double-click, Enter, the panel's Start / Resume and menu items start it; group rows never launch | `view_vs_start`: a click on a stopped session shows its details with Resume and launches nothing (service log, no tile); a double-click, Enter and the Resume button each resume one (the stand-in Copilot); a group row shows running terminals and launches nothing | steps "a click views a stopped session without launching it", "a double-click on the box starts it", "the details panel's Start button starts a session"; canvas-core `interact` tests | ✓ |
| 35 | **New:** transcript inspector for session and subagent boxes (live while they run); a finished child resumes as its own linked subsession, never beside a writer | `inspector_and_child_resume`: a Copilot session's transcript and a subagent's note; an OpenCode child (synthetic database) that works, whose inspector shows a new line written while it is open; with its parent running (stand-in OpenCode) a finished child offers no resume; parent stopped: **Resume as subsession** adds a linked box and the service resumes it with `-s <child>` | canvas-agents `transcript` tests (Copilot with subagent events, OpenCode with a child, Claude Code with a subagent file), canvas-core `inspect` tests (resume rules, a refused resume), `backend_contract` (local and remote answers match); `selftest agent --agent opencode --prompt` (inspect and resume a real child) | ✓ |
| 36 | **New:** removing a multi-selection: right-click on a selected box (or Del) removes its eligible boxes after a confirmation; sessions running in a canvas terminal stay; history is never deleted; Ctrl+Z restores them | `multi_remove_keeps_running`: a 3-box selection (one running) → menu “Remove 2 from canvas” → the confirmation names the running one → two boxes gone, their history files kept → Ctrl+Z brings them back | canvas-core `remove` tests (eligibility, subagents go with their session, subsessions unlinked, undo) | ✓ |
| 37 | **New:** a click on a group's empty background or its border selects the group (innermost; a few pixels outside the outline in the GUI); title drag, ◢ resize, folder chips, background pan and double-click are unchanged | `group_background_and_border_select`: an empty background spot and a point 0.2 cells outside the outline each select the group (its panel, nothing launched); a background drag pans; title drag and ◢ still move and resize; chips stay clickable; a double-click opens the new-session dialog | step "a click on a group's border selects the group" (heavy outline, its panel); canvas-core `interact` tests (both render modes, nested groups, gestures) | ✓ |

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
21   | Activity bar views (Alt+1…8)                                   | activity_views_alt_1_to_8                    | ✓
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
G1   | New: inline images (Kitty graphics; iTerm2 OSC 1337 through C… | kitty_image_pixels_png (GUI only: the TUI c… | ✓
G2   | New: taskbar progress (ITaskbarList3), FlashWindowEx, toasts   | taskbar_flash_toasts (GUI only: a console h… | ✓
G3   | New: file drag and drop (paths pasted; uploaded in remote mod… | drop_files_local (GUI only: a console canno… | ✓
G4   | New: several windows on one canvas (Ctrl+Shift+N, toolbar)     | second_window_rename_sync (GUI only: a cons… | ✓
PASS: 38 rows in 21.8s
```

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
