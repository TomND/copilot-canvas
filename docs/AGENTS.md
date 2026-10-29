# Agents

The canvas hosts three agent CLIs: **GitHub Copilot CLI**, **OpenCode** and
**Claude Code**. Each is a `Provider` (`crates/canvas-agents/src/agents/`):
it builds the command line of a launch (new, resumed, forked or headless),
reads the agent's own history store for status, subagents and git/PR events,
and lists the agent's configuration. Everything agent-specific goes through
the provider; the canvas, the service and the event bus are shared.

Every launch also gets the canvas integration: `COPILOT_CANVAS_HOME`,
`COPILOT_CANVAS_SESSION` (the box id) and the `copilot-canvas` command on
`PATH` (events, messages, passive status), plus the canvas skills
(`canvas-events`, `canvas-passive`).

## At a glance

| | Copilot CLI | OpenCode | Claude Code |
|---|---|---|---|
| Session id | the box id (a UUID), chosen by the canvas | `ses_…`, chosen by OpenCode on the first prompt; the box keeps its UUID and learns the `ses_` id (*re-keying*) | the box id (a UUID), chosen by the canvas |
| New | `copilot --session-id ID [--name N] [-i PROMPT]` | `opencode [--prompt P] --port PORT` (server password in its environment) | `claude --session-id ID [-n N] [PROMPT]` |
| Resume | `--session-id ID` again | `opencode -s ses_…` | `claude -r ID` |
| Fork (subsession) | history copied on disk like `/fork`, then resumed | `opencode -s PARENT --fork` (the fork's id is learned like a new session's) | `claude -r PARENT --fork-session --session-id NEW` |
| Allow-all (default) / ask | `--allow-all` / nothing | `--auto` / nothing | `--dangerously-skip-permissions` / nothing |
| Model, agent | `--model` (new sessions; passive runs every time), `--agent` (new sessions) | `-m` (new sessions; passive runs every time), `--agent` (new sessions) | `--model` (new sessions; resumes and forks with the session's recorded model; passive runs every time), `--effort`, `--agent` (new sessions) |
| MCP servers off | `--disable-mcp-server NAME` | `OPENCODE_CONFIG_CONTENT` `{"mcp": {NAME: {"enabled": false}}}` | a private `--strict-mcp-config --mcp-config` file without them |
| Passive run | `copilot --session-id ID -p PROMPT --allow-all --no-ask-user` | `opencode run PROMPT [-s ses_…] --auto -m MODEL` | `claude -p PROMPT --session-id/-r ID --dangerously-skip-permissions` |
| History | `~/.copilot/session-state/<id>/events.jsonl` (tailed) | `~/.local/share/opencode/opencode.db` (SQLite, read-only, indexed lookups only) | `~/.claude/projects/<dir>/<id>.jsonl` (tailed) |
| Live state | OSC 9;4 progress; `inuse.<pid>.lock` | the TUI's own HTTP server (busy, permission prompts, questions) | `~/.claude/sessions/<pid>.json` (busy / idle / waiting) |

Stopping a session sends Ctrl+D (twice), then Ctrl+C, and ends its process
tree after 5 s. Before an OpenCode TUI is asked to quit, the canvas stops
polling its server and waits for requests in flight (OpenCode aborts
half-finished requests when it quits and then hangs).

## Models

Model ids differ per agent, so the canvas keeps a default per agent
(`LaunchDefaults::models` in the canvas document: agent id → model id; an
empty id means the agent's own default, no model flag). A new session gets,
in this order: the model picked for it in the new-session dialog, its group's
override (*Session defaults for group…*), the canvas's remembered default for
its agent, else the initial default, **Claude Opus 5.5**
(`canvas_model::models`):

| | Copilot CLI | OpenCode | Claude Code |
|---|---|---|---|
| Initial default | `claude-opus-5.5` | `github-copilot/claude-opus-5.5` | `claude-opus-5-5` |
| Where the id comes from | GitHub's model id, dotted (the same id OpenCode's `github-copilot` provider lists; Copilot CLI is not installed on the development machine, so `copilot --help` was not seen; any id can be typed) | `opencode models` (1.19.4) lists it; the owner's `~/.local/state/opencode/model.json` has it among the recent models | Claude Code 2.1.309's own model list has `claude-opus-5-5`; `--help` takes "a model's full name" or an alias (`fable`, `opus`, `sonnet`), but `opus` names a different model as releases change |
| Model list in the picker | the `(choices: …)` of `--model` in `copilot --help`, when this version lists them | `opencode models` | the aliases in `--help` |

**The dialog** shows the model before starting, with why: *from group X*,
*remembered default for …*, *initial default (Claude Opus 5.5)*, or *your
choice*. Its picker offers the current model, *Type a model id…*, the initial
default, *(agent default)* and the agent's list; typing filters, and an id
that matches nothing is used as typed (one word, never starting with `-`).
A model picked there starts that session and, when you create it, becomes its
agent's remembered default (`Doc::defaults`, so it is saved, synced to every
window and kept across restarts) until you pick another there or in Settings
(*Copilot / OpenCode / Claude Code model*; *(not set)* goes back to the initial
default). Picking the default again is no new choice. In a group with its own
model a pick applies to that session only: neither the group's override nor
the canvas default changes (the choice was made against the group's model,
and changing the canvas's from inside a group would surprise). A fork shows
and keeps its parent's model.

**Each session records** the model it started with on its box
(`Node::model`, set when it is created or first launched; empty for the
agent's own default; boxes from before are unrecorded) and the details panel
shows it. A new default never changes an existing session:

- **Copilot**: a resume (`--session-id` again) carries no `--model`; Copilot
  keeps the session's model itself.
- **OpenCode**: a resumed or forked TUI (`-s`, `-s … --fork`) gets no `-m`:
  when it opens the session its prompt takes the model (and agent) of the
  session's last message, which wins over `-m` anyway (OpenCode 1.19.4's TUI
  sets its model from `-m` only as a fallback and asks to add it to its
  recent models; in the agent check `model.json` stayed unchanged, and the
  check puts it back if it ever changes). A box whose TUI never got a prompt
  has no session yet; the service starts it as a new one, with its recorded
  model.
- **Claude Code**: nothing is saved with its sessions, so a resume (`-r`) and
  a fork get `--model` with the box's recorded model, or none when it was not
  recorded; never the current default.

The canvas sends the conversation's model in every launch (`AgentLaunch::model`:
the one to start with, or the recorded one for a resume); each provider passes
it where its agent needs it. The service log names the model only when the
command line carries it (`terminal …: claude resume, model claude-opus-5-5`).

**Passive runs** use the model new sessions of their agent get in their group
(`PassiveSpec::model`) on every run; a run that resumes a conversation whose
model is recorded on its box (you started its chat) uses that one
(`PassiveSpec::session_model`), and a rollover starts the next conversation with
the current default.

**Older canvases** had one `model` for every agent. It moves to the agents
whose ids it fits when the canvas is read: a `provider/model` id to OpenCode,
any other id to Copilot and Claude Code (which both got it before).

## How status is derived

`canvas-core/src/status.rs` turns a session summary (from history), the
terminal (alive, exit code, OSC 9;4 progress) and when you last looked at the
box into one state: **working**, **needs you**, **done · review**, **idle**,
**stopped**, **failed**, **open elsewhere**. A finished turn stays *done ·
review* until you open, click or type in that session; a turn that ends while
you watch its terminal is already reviewed.

- **Copilot**: its progress report starts and ends turns at once; the events
  add pending permissions/questions, failures, subagents and schedules.
- **OpenCode**: the newest messages and the parts of the newest message
  (unfinished assistant message or running tool: working; `question` tool
  running: needs you; error: failed; Esc abort: idle with nothing to review)
  overlaid with what the TUI's own server says (busy, permission prompts and
  questions, including a subagent's, which count for its root session). The
  server is authoritative while it answers: busy means working; not busy
  ends a turn the database still shows as unfinished.
- **Claude Code**: the transcript (tool calls, `end_turn`, `turn_duration`)
  and the live pid file (`busy`, `waiting` with what it waits for).

Subagents become boxes linked to their session: Copilot's `subagent.*`
events, OpenCode's child sessions (`parent_id`; *general* or *explore* from
the "(@agent subagent)" title), Claude Code's `Agent` tool calls (background
ones end through their `subagents/agent-*.meta.json`).

Git pushes and pull-request actions found in tool calls (`git push`, `gh pr
…`, `az repos pr …`, the Azure DevOps and GitHub pull-request tools) become
bus events (`pr.pushed`, `pr.created`, …) of the box's group. For OpenCode the
service maps the `ses_` id back to the box, since routing knows boxes.

## Inspecting and resuming child conversations

Agents start conversations of their own; the canvas shows each kind:

| | What it is | On the canvas | Inspect reads | Resumes on its own |
|---|---|---|---|---|
| Spawned session | a session an agent started inside a canvas terminal (`copilot -p …`, found in the terminal's process tree: `ServiceMsg::Subsession`) | a linked subsession box (dotted connector) | its own history | yes, like any session box (`--session-id`), unless it is still open (its in-use lock) |
| OpenCode child | a task-tool child session (`parent_id` in OpenCode's database) | a subagent box (`<box>:<ses_ id>`) | the child session's messages and parts | yes, as a **new linked subsession box** (`opencode -s <child>`), when nothing could write to it |
| Claude Code subagent | an `Agent` tool call; background ones write `subagents/agent-<id>.jsonl` beside the transcript | a subagent box (key: the tool call id) | that file (`agentId` from the tool result), else the task and its answer | no: part of the parent's conversation |
| Copilot subagent | `subagent.*` events in the parent's `events.jsonl`; its own steps carry its `agentId` | a subagent box | the parent's events with its `agentId` (older Copilot: only its start and end) | no: part of the parent's conversation |

**Inspect** (box menu, details panel, any session or subagent box) opens a
read-only transcript: your prompts, the agent's answers, its tool calls (name,
a few words of arguments, the start of the result or the exit code) and notes
(a subagent started or finished, an error, an Esc), oldest first, newest at the
bottom, scrollable, with Copy and Close. It comes from the backend
(`Request::Transcript`, see BACKEND.md), so it works for remote clients too,
and reads are bounded: the newest 2 MiB of a JSONL history and the newest 150
OpenCode messages through the database's indexes (large text and tool output
are cut in SQL; a database without the indexes is not read). While the
conversation runs (an unfinished turn or an open tool call) it refreshes every
1.5 s, otherwise every 5 s. Nothing is started and nothing is written.

**Resume after completion.** A finished OpenCode child resumes only when
nothing can be writing to it: its parent is not running in this canvas (its
OpenCode could continue the child with the task tool), not open elsewhere, and
the child itself is not open elsewhere. Then the inspector's and the details
panel's **Resume as subsession** (or a double-click on the subagent box) adds
a new subsession box linked to the parent whose `agent_session` is the
child's id and resumes it in its own terminal; the subagent box stays as it
is, and a second resume goes to the same box. The service checks again right
before the launch (`is_live`); when it refuses, the new box goes again and a
notice says why. The parent is never signalled. Copilot and Claude Code
subagents are part of their parent's conversation: the inspector says so, and
resuming means resuming the parent.

## OpenCode in detail

**Its server.** Every interactive OpenCode launch gets `--port PORT` and a
random password (`OPENCODE_SERVER_PASSWORD`); the port and password stay in
the service. The port is chosen by binding port 0 and releasing it, so another
process could take it before OpenCode does. Before the first authenticated
request the service checks with `GetExtendedTcpTable` that every loopback or
wildcard listener on that port belongs to the terminal's process tree; until
then nothing (and so no password) is sent, and a foreign listener is logged
once (`opencode: port … is held by …`). The check repeats after any failed
request. The watcher asks `/path` once, then `/session/status`, `/permission`
and `/question` (scoped to the TUI's folder) every second.

**Re-keying.** A new TUI has no session until its first prompt. The service
keeps the terminal *pending*: once the TUI's server told it the folder, it
looks for new root sessions created there since the launch. A session the
TUI's own server reports busy or waiting is its own; otherwise the oldest new
one counts once it is 10 s old, the terminal exited or it is a fork, unless
another pending terminal works in the same folder. The service then logs
`terminal ID runs agent session ses_…`, broadcasts it, and the client sets the
box's `agent_session`: history lookups, tracking, the resume (`-s`) and
passive runs use the `ses_` id from then on, while the box (and its terminal)
keep their id. A resumed session's id is known at launch. A session counts as
open in a TUI only when that TUI's server reported it busy or waiting, or the
service gave it to that TUI (sessions other processes create in the same
folder are not), so it is never resumed twice and never blocked by a
neighbour.

**Passive runs.** `opencode run` with the passive prompt, `--auto` (allowed
tools never ask), the group folder allowed as an external directory, and
`-m` with the model new OpenCode sessions in the group get (see
[Models](#models)). Its session id is learned from the folder after the
run starts; later runs resume it with `-s`.

## Verified end to end

`copilot-canvas selftest agent [--agent copilot|opencode|claude]` starts the
real TUI from the canvas without a prompt: imports, configuration views, the
dialog's model (the initial default), the allow-all flag and the model flag
on the command line (and the model on the box), typing, a graceful stop. For
OpenCode it puts `model.json` (its recent models) back afterwards if the
launch changed it. With `--paste` it pastes multi-line
text into the agent's input box instead and checks that it arrived as lines
and nothing was sent ([Pasting](#pasting)).

`copilot-canvas selftest agent --agent opencode --prompt [--model ID]
[--client tui|gui] [--steps LIST]` sends tiny prompts, only to a model whose id
ends in `-free` (default `opencode/deepseek-v4.1-flash-free`), only into a TUI
whose command line carries `-m` with that model, and with OpenCode's defaults
(`model`, `small_model`, the *general* and *explore* agents) pointed at it as
well. Through the canvas it checks:

| Step | What is checked |
|---|---|
| status | Settings → OpenCode model sets the model; the dialog starts OpenCode with `-m` and `--auto`; the service reaches the TUI's verified server; after the first prompt the box follows the new `ses_` session (the canvas file and the service log), shows working, then *done · review* while you look elsewhere, then idle once you open it |
| wheel | after an 80-line answer, three wheel notches over the terminal scroll OpenCode's own view (earlier lines appear) |
| push | `cd repo; git push origin main` (a separator every shell knows) to a local bare repository in OpenCode's bash tool: `detected pr.pushed in ses_… (terminal BOX)` in the service log and a `pr.pushed` bus event whose source is the box |
| subagent | the task tool: each child session becomes a subagent box under the session (skipped with a note when the model does not use the tool); its box's **Inspect transcript** opens the child's transcript read-only and launches nothing |
| question | the question tool: the box shows *needs you*; Esc (OpenCode's "Reject question") dismisses it and the box settles to idle |
| resume | X stops the box gracefully; a click only shows its details (Resume), the double-click resumes with `-s ses_…` (no `-m`: OpenCode keeps the session's model) and shows the conversation. Before that, with the parent stopped, the first child resumes as a linked subsession with `-s <child>` (no prompt) and is stopped again |
| permission | Settings → Permissions: ask; a new box without `--auto`; a bash call with `OPENCODE_PERMISSION` `{"bash":"ask"}` shows *needs you*; Esc ("Reject") rejects it and nothing ran |
| passive | a passive OpenCode box (Run now) runs `opencode run … -m <free model>` headless, reports `copilot-canvas status ok "…"` through the canvas command on its PATH, and its box follows the run's `ses_` session |

At the end every OpenCode session it created (subagents included) is reported
with the models it used and its cost, deleted with `opencode session delete`,
and checked to be gone. `busystop` and `streamstop` (extra steps) stop the box
while a tool runs or the answer streams. Unit tests cover the command lines,
the listener check, history parsing, re-keying decisions and status mapping.

## Pasting

A paste (Ctrl+V or Ctrl+Shift+V, the pane menu's *Paste*, Windows Terminal's
paste into the TUI, IME text with a line break in the GUI) never sends a
message:

- The client sends one `Input::Paste` without the text's trailing line breaks
  (a copied line that ends with a newline is not submitted; line breaks alone
  paste nothing).
- The service encodes it for the program's current modes, the same way with
  both engines (`canvas_host::keys::paste_lines`): paste markers inside the
  text are removed (also ones that removing another would form); CR LF, CR
  and LF are line breaks; control characters that would act as keys (NUL,
  Ctrl+C/D/E/O/Q/R/S/U/V/W/Z, BS, ESC, FS, DEL) become spaces, as in Ghostty
  and xterm.
  - **Bracketed paste on** (the agents turn it on for their input box): one
    `ESC [200~ … ESC [201~` whose lines end with CR, as Windows Terminal and
    xterm send them.
  - **Off:** the lines joined with the program's "insert a line break" key,
    never a bare CR (Enter): Shift+Enter `CSI 13;2u` when the program uses
    the kitty keyboard protocol, else Alt+Enter (ESC CR).
- In the TUI, the console hands a paste to the canvas as key presses (it
  strips Windows Terminal's paste markers). A burst of text keys that contains
  a line break and comes as press/release pairs in one read, or continues such
  a burst within 8 ms (a paste split across reads), is one paste
  (`canvas_core::input::coalesce`), its final line break included. Typed keys
  arrive one per read, or overlap, or end with a key still held: typing and
  its Enter stay keys.

What the programs did with it (2026-10-29; `copilot-canvas selftest agent
--paste --agent opencode|claude`, nothing submitted; the modes are the
service engine's while the input box was up):

| Program | Modes | Four lines (text, blank, indented, text) and a trailing line break | Two lines, the second indented | Sent? |
|---|---|---|---|---|
| OpenCode 1.19.4 | bracketed paste, kitty keyboard (flags 5) | `[Pasted ~4 lines]` in the input box | both lines, indentation kept | no: no session in OpenCode's database |
| Claude Code 2.1.309 (throw-away `CLAUDE_CONFIG_DIR`) | bracketed paste, kitty keyboard (flags 5) | `[Pasted text #1 +3 lines]` | both lines, indentation kept | no: no transcript |
| Copilot CLI | not checked here (no Copilot CLI on the development machine); it uses bracketed paste and the kitty keyboard protocol like the others, so it gets the same bracketed paste | | | |
| PowerShell 5.1 (PSReadLine; `selftest`, parity row 14) | neither | the lines arrive joined on one line (PSReadLine has no Alt+Enter binding) | | no: nothing runs |
| cmd.exe | neither | its line input ends a line on any Enter, Alt+Enter included: every line but the last runs (Windows Terminal runs them all) | | yes: cmd has no multi-line input |
| a program with bracketed paste (the paste probe; `selftest`, parity row 14) | bracketed paste | one paste, CR between the lines, blank lines and indentation kept | | no Enter outside the paste |

Behind Windows' inbox ConPTY, a program that reads VT input (the agents, the
probe) receives the paste markers, CR and LF unchanged; a program that reads
console key events (PSReadLine, the TUI) gets the pasted characters as key
presses without the markers, ESC CR as Alt+Enter, LF as Ctrl+Enter
(PSReadLine: insert a line *above*) and `CSI 13;2u` not at all; a Win32
input-mode Shift+Enter would give PSReadLine a proper new line but reaches VT
readers as a bare CR, so it is not used. `copilot-canvas paste-probe
[--plain] [--kitty] [--keys] [--raw]` (hidden) prints what a program
receives; `paste-probe --drive TEXT [--program "CMD"] [--ready TEXT]` runs it
(or CMD) in a hidden pseudoconsole, writes TEXT (`\e`, `\r`, `\n`, `\xNN`)
and prints the screen.

## Known limitations

- OpenCode: a TUI that starts a second session (`/new`) keeps the box on its
  first one. A session opened in an OpenCode TUI the canvas did not start is
  not seen as open elsewhere (only the canvas's own TUIs have a server it can
  ask). A passive OpenCode run whose group folder also has a canvas OpenCode
  TUI that has not had a prompt yet learns its session id only after that TUI's
  first prompt.
- Status from OpenCode's server lags its screen by about a second (the server
  is asked once a second); the first prompt's session is learned within 1–3 s.
- Claude Code is never sent a prompt by the tests; its status paths are
  covered by unit tests and the no-prompt check.
- Copilot's real-agent check needs the Copilot CLI, which the development
  machine does not have; it passed on the owner's Cloud PC (2026-10-28), and
  Copilot's paths are also covered by the demo self-tests (a stand-in CLI) and
  unit tests.
