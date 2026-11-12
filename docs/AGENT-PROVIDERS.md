# Agent providers

(How the canvas runs the agent CLIs. Instructions for agents working on this repository are in the root [AGENTS.md](../AGENTS.md).)

The canvas hosts three agent CLIs: **GitHub Copilot CLI**, **OpenCode** and
**Claude Code**. Each is a `Provider` (`crates/canvas-agents/src/agents/`):
it builds the command line of a launch (new, resumed, forked or headless),
reads the agent's own history store for status, subagents and git/PR events,
and lists the agent's configuration. Everything agent-specific goes through
the provider; the canvas, the service and the event bus are shared.

Every launch also gets the canvas integration: `ATELIER_HOME`,
`ATELIER_SESSION` (the box id), `ATELIER_AGENT` with
`ATELIER_SESSION_AGENT` (the session's agent, see [Notes and
todos](#notes-and-todos)) and the `atelier` command on
`PATH` (events, messages, passive status, reports, pull requests, review
follow-ups, notes, todos), plus the canvas skills (`canvas-events`,
`canvas-passive`, `canvas-reports`, `canvas-prs`, `canvas-review-followup`,
`canvas-todos`, `canvas-notes`; plugin 1.5.0).

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
| Canvas skills of a canvas-wide agent | another `--plugin-dir <agent>/plugin` (Copilot takes the option several times) | `<agent>/plugin/skills` added to `skills.paths` in `OPENCODE_CONFIG_CONTENT` | another `--plugin-dir <agent>/plugin` (`.claude-plugin/plugin.json`; repeatable) |
| Group skill folders | another `--plugin-dir <home>/group-skills/<instance>/<group>/<n>-<terminal>` | that folder's `skills` added to `skills.paths` | another `--plugin-dir` (the same folder) |
| History | `~/.copilot/session-state/<id>/events.jsonl` (tailed) | `~/.local/share/opencode/opencode.db` (SQLite, read-only, indexed lookups only) | `~/.claude/projects/<dir>/<id>.jsonl` (tailed) |
| Live state | OSC 9;4 progress; `inuse.<pid>.lock` | the TUI's own HTTP server (busy, permission prompts, questions) | `~/.claude/sessions/<pid>.json` (busy / idle / waiting) |

Stopping a session sends Ctrl+D (Copilot CLI's *Shutdown*) at once and again
after 2 s, Ctrl+C twice after 3.5 s (Copilot and Claude Code exit on a second
Ctrl+C), `/exit` and Enter after 6 s (each agent's quit command, for one whose
keys a prompt took), and ends its process tree after 10 s (`STOP_GRACE`; it
was 5 s, and on `service stop` every Copilot of a busy host was killed with
code 1); each step only while the program still runs. Before an OpenCode TUI is asked to quit, the canvas stops
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

## Canvas-wide agents and their skills

A canvas-wide agent (README, *Canvas-wide passive agents and skills*) is a
session box with one conversation that both its chat and its runs use.

**Runs** are like a group passive session's: headless turns of its agent
(the passive run row above), started in its own folder
`<home>/canvas-agents/<canvas id>/agents/<slug>` (OpenCode learns each run's
`ses_` id from the folder a run starts in, so agents never share one) with
the canvas-agents folder `<home>/canvas-agents/<canvas id>` allowed
(`--add-dir` for Copilot and Claude Code, `external_directory` for OpenCode),
so it reads the canvas's `GOALS.md` and skills. Its prompt starts with
`[Atelier · canvas-wide agent "<name>"]` and its triggers carry the
sending session's id and group. A run uses the agent's own model
(`Passive::model`, routed as `PassiveSpec::model`) or else the one new
sessions of its agent get; a resumed conversation whose model is recorded on
the box keeps that one (`PassiveSpec::session_model`; setting the agent's
model records it on the box too, so the current conversation follows). An
agent with no triggers (no events, no schedule) gets no events or timers: it
runs only on messages, addressed events, follow-ups it scheduled and Run now.

**Its chat** is the explicit start of that conversation (a double-click,
Enter, *Chat*, *Create & start*): an interactive launch of its agent in its
folder (new: `--session-id ID` / OpenCode's TUI / `claude --session-id ID`;
later: a resume, like a session's), with the canvas-agents folder in
`add_dirs` (Copilot takes it for new conversations; Claude Code and OpenCode
on every launch) and its skills plugin (below). A new conversation's first
prompt (`AgentLaunch::prompt`: `-i` for Copilot, `--prompt` for OpenCode,
positional for Claude Code) says who it is, what wakes it and where its
instructions (`rules.md`), memory and the canvas goals are, then your first
message (or asks it to say what it will do). Never two writers: while a
headless run of it is in progress (`PassiveView::running`) the window does
not open the chat; it opens it once the run has ended and its terminal is
gone. A run that comes while the chat is open is typed into the chat when it
is idle (not working, no question pending, nothing typed for 20 s), else
retried; a run never starts a new conversation beside one its chat began: the
service resumes a conversation that already has history even when no run
has started it yet (`fresh_conversation` in `service/passive.rs`). Changing
its agent CLI gives the box a new id and the agent a new routing key: a fresh
conversation (the old one stays an earlier episode), its files unchanged.

**Skills.** A canvas skill is `skills/<name>/SKILL.md` in the canvas-agents
folder: YAML front matter with `name` (lowercase, digits, single hyphens; the
folder's name) and `description` (when the skill applies), then Markdown
instructions, plus any files beside it. Before every run, and before its chat
starts from the canvas, the service copies the agent's assigned skills (at
most 200 files, 8 MB, 4 levels each; no links) into the agent's `plugin/`
folder and writes the two manifests one folder serves:

| | Copilot CLI | OpenCode | Claude Code |
|---|---|---|---|
| Manifest | `plugin.json` (`name: canvas-agent-<slug>`, `skills: ./skills/`) | none: the folder is read directly | `.claude-plugin/plugin.json` (`name: canvas-agent-<slug>`) |
| How the run gets it | a second `--plugin-dir <agent>/plugin` after the canvas's own plugin | `skills.paths` gets `<agent>/plugin/skills` after the canvas's own skills folder | a second `--plugin-dir <agent>/plugin` |
| Also | the prompt lists the skills (name, description) and their folder, so an agent CLI that ignored the plugin could still read them | same | same |

Skills taken away disappear from the plugin at the next run; with none the
plugin folder is removed. A skill the library no longer has is logged by the
service (`canvas agent …: skills not in the canvas library: …`) and marked
*(missing)* in the agent's panel. The service log line of the launch ends in
`, with its canvas skills` when a run (or chat) got the plugin. Group passive
sessions and ordinary sessions get no skills of the canvas library; they get
the group skill folders instead (below).

## Where agents find skills

Each agent CLI reads skills by itself from project and personal folders. The
canvas does not change that; the ✦ Skills view lists what each agent finds
(`canvas_model::skills`, `Request::SkillSources`). The folders, as each
agent's documentation gives them (read 2026-11-01):

| | Project folders (a session started in folder F of a repository) | Personal folders | Source |
|---|---|---|---|
| Claude Code | `.claude/skills/<name>/SKILL.md` in F and every parent up to the repository root (deeper ones only once Claude works on files there) | `~/.claude/skills` (`CLAUDE_CONFIG_DIR` when set) | [code.claude.com/docs/en/skills](https://code.claude.com/docs/en/skills) (*Choose where skills load*, *Load skills in monorepos and subdirectories*) |
| OpenCode | `.opencode/skills` (also the singular `.opencode/skill`), `.claude/skills` and `.agents/skills`, from F up to the git worktree | `~/.config/opencode/skills` (and `skill`), `~/.claude/skills`, `~/.agents/skills` | [opencode.ai/docs/skills](https://opencode.ai/docs/skills) (*Place files*, *Understand discovery*); singular folder names: [opencode.ai/docs/config](https://opencode.ai/docs/config) |
| Copilot CLI | `.github/skills`, `.agents/skills`, `.claude/skills` of the repository, and `.github/skills` of parent folders (monorepos) | `~/.copilot/skills` (`COPILOT_HOME` when set), `~/.agents/skills`; not `~/.claude/skills` | [About agent skills](https://docs.github.com/en/copilot/concepts/agents/about-agent-skills), [Adding agent skills for Copilot CLI](https://docs.github.com/en/copilot/how-tos/copilot-cli/customize-copilot/add-skills), [CLI command reference › Skills reference](https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-command-reference) |

The repository root is the nearest folder at or above F holding `.git` (a
folder, or a linked worktree's file); without one, F itself. For Copilot CLI
the canvas reads `.agents/skills` and `.claude/skills` in F and at the root,
`.github/skills` in every folder between. The listing looks a few levels below
each skills folder (as `~/.claude/skills/synced/<account>/<skill>`, which Claude
Code and OpenCode load) and leaves out hidden folders such as Claude Code's
`.trash`. Same names: Claude Code prefers a personal skill to a project's,
plugin skills keep their plugin's name (`plugin:skill`), Copilot CLI uses the
first found in its order, OpenCode asks for unique names; the view marks a
skill with `≈` when an agent that loads it also loads another of its name.
Plugins, built-in skills and enabled/disabled state come from each agent's own
listing (`ConfigSnapshot`), merged into its personal folders.

What the listing reads (on the service's machine, never written): the canvas
plugin (`<home>/plugin/skills`), the skill folders of every session and each
group, the projects of the canvas's sessions (the distinct folders of its
session boxes, from their history when a box has none, and the group folders;
at most 40 project roots), the personal folders of the installed agents and
the canvas library. Each SKILL.md is read for its front matter (`name`,
`description`) from its first 8 KB; at most 200 skills a folder and 2,000
folders looked at a scan.

## Group skill folders

A group's skill folders (*Skill folders…* in the group menu, *Edit skill
folders…* in its details; the canvas's own in *Settings › Skill folders (all
sessions)…*; `LaunchDefaults::skill_dirs`) are folders of skills (`<name>/SKILL.md`
below them) or one skill (a folder with a SKILL.md). The canvas's and the
group's lists both apply, the canvas's first, each folder once. Every
session the canvas starts in the group (new, resumed, forked, subsessions,
a passive session's chat) and every run of the group's passive sessions gets
them, with all three agents; canvas-wide agents keep their own skills.

| | Copilot CLI | OpenCode | Claude Code |
|---|---|---|---|
| Manifest | `plugin.json` (`name: canvas-group-skills`, `skills: ./skills/`) | none: the `skills` folder is read directly | `.claude-plugin/plugin.json` (`name: canvas-group-skills`) |
| How the session gets it | one more `--plugin-dir <home>/group-skills/<instance>/<group>/<n>-<terminal>` | `skills.paths` gets that folder's `skills` | one more `--plugin-dir` (the same folder) |

Right before the launch the service copies the skills into a new folder of
that launch (`<instance>`: the service's instance name, `<n>`: a launch
counter; the canvas library's limits: 200 files, 8 MB and 4 levels a skill,
each folder's files first, no links; at most 100 skills and about 64 MB a
launch). The previous launch's folder goes when the terminal launches again,
the folder of a launch when its terminal exits (an exit handled after a
relaunch never takes the relaunch's), and the instance's whole folder when
the service starts (services of other instances sharing the home keep
theirs). Nothing outside `<home>/group-skills/<instance>` is written or
removed. Left out, and named in the service log (`terminal …: skill folders
of group … (N folder(s)): a, b; skipped c (the session's project already has
it)`): names the agent already loads from the session's own folder (its
project folders above), a name an earlier folder gave, folders that are not
full paths, more than 40 folders (the canvas's 20 and the group's 20), links
below the folders chosen (a folder chosen may itself be a link), skills whose
SKILL.md was not copied, and what does not fit. The launch's log line then
ends in `, with its group skills`. The editor checks a folder on the service's
machine in the background (the window never waits for it) and takes at most
20 a list.

**Carrying them.** `AgentLaunch` travels postcard-encoded, so it gets no new
field: the client puts the folders and the group's id into its environment as
`ATELIER_GROUP_SKILLS` (JSON `{"group": …, "dirs": […]}`;
`canvas_model::launch`), and the service takes that entry out (its name in any
case, as Windows reads it; one that is not valid JSON is dropped and the
launch goes on without folders) before the agent starts; plain Copilot
launches (`LaunchKind::CopilotWith`) never pass it on either. An older service
passes it on to the agent as a variable that does nothing. Passive runs get
them from the routing (`PassiveSpec::skill_dirs`, JSON with a default; an
older service ignores them).

## How status is derived

`canvas-core/src/status.rs` turns a session summary (from history), the
terminal (alive, exit code, OSC 9;4 progress) and when you last looked at the
box into one state: **working**, **needs you**, **done · review**, **idle**,
**stopped**, **failed**, **open elsewhere**, **↯ interrupted** and
**resuming after restart**. A finished turn stays *done · review* until you
open, click or type in that session; a turn that ends while you watch its
terminal is already reviewed.

**Interrupted** (`canvas_model::status::was_busy`): no terminal here, not live
elsewhere (Copilot's `inuse.<pid>.lock` with a process started before the
lock was written, Claude Code's pid file with its process start, OpenCode's
answering server), yet the history says it was working, waiting on a
question or permission, looping (`loop_ms`) or running a subagent. A clean
exit (`session.shutdown`, an idle transcript) is *stopped*; a terminal of
this service that ended (you stopped it) stays *stopped* or *failed*. Its
loop is drawn broken and still. **Resuming after restart** comes from the bus
snapshot's `restore` record while the service resumes the box after a
restart (README "Restore after a restart"); the resume is the provider's own
(`--session-id`, `-s`, `-r`) through the normal launch path, and the note the
service types in once it has settled asks the agent to continue its task,
set up its loop again and ask again what it was waiting on.

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

## Reports from agents

Any agent (an ordinary or passive canvas session of any of the three CLIs, or
a scheduled task outside the canvas) leaves something for the user to read
with one command; the `canvas-reports` skill (plugin 1.1.0, next to
`canvas-events` and `canvas-passive`, which both point to it) teaches it:

```text
atelier report publish (--markdown FILE|- | --html FILE|- | --message TEXT) [--title TITLE]
               [--kind info|ok|warn|alert|queued|delivered|failed] [--pr URL] [--link TITLE=URL]…
               [--session NAME|ID] [--thread KEY] [--source NAME]
atelier report list [--unread] [--limit N]
```

| | |
|---|---|
| Publisher | `ATELIER_SESSION` (the box; passive runs too), else `--source NAME` or `AUTOMATION_NAME`; with none it refuses ("this is not an Atelier session: name who publishes with --source NAME") |
| Body | Markdown, HTML (a whole page with its own styles; images inline as `data:` URIs) or a message of at most 4,000 characters; at most 2 MiB, UTF-8, not empty |
| Title | `--title`, else the first `#` heading, the HTML `<title>`/`<h1>`, the message's first line; one line, 160 characters |
| Links | `--pr URL`, `--link TITLE=URL` (≤ 20): `http(s)://` or `mailto:` only, ≤ 2,048 characters |
| Kind | `info` (default), `ok`, `warn`, `alert`, `queued` (asked for), `delivered` (confirmed), `failed` |
| Thread | `--thread KEY` (`A-Z a-z 0-9 . _ : / # -`, ≤ 120): later updates join one inbox row (e.g. `pr-123`: queued, then delivered) |
| Answer | `Report stored in the Atelier inbox: “T” (id).` (exit 0); `Report queued, not stored yet: …` (exit 0, no service running: it is stored when the service starts); `Atelier refused the report: …` or a check's reason (exit 1) |

The command checks everything before it writes; then it spools the
submission (`<home>/reports/spool/<id>.json`, written to a temporary file and
renamed) and, when the service is alive (`reports/service.json`), waits up to
10 s for it to be stored or refused. The service's inbox thread validates it
again, resolves the publisher's box name and group from routing, and stores
it; nothing is ever started. The skill has an example morning brief and
short message, says how to check the result (never assume a report arrived),
and its rules: lead with what needs the user, keep messages to one or two
lines, no secrets, and reports are for reading (links, not commands to run).

## Pull request coordination

Every launch gets the `canvas-prs` skill (plugin 1.2.0, beside
`canvas-events`, `canvas-passive` and `canvas-reports`; `canvas-events` and
`canvas-passive` point to it). It teaches two roles:

**A working session that published a pull request** hands it off at once:

```text
atelier pr track URL [--session NAME|ID] [--title TEXT]
atelier pr untrack URL · pr check [URL] · pr list [--format json]
```

| | |
|---|---|
| Links | GitHub and GitHub Enterprise `https://HOST/OWNER/REPO/pull/N[/…]`, `OWNER/REPO#N`; Azure DevOps `https://dev.azure.com/ORG/PROJECT/_git/REPO/pullrequest/N`, `https://ORG.visualstudio.com/[DefaultCollection/]PROJECT/_git/REPO/pullrequest/N`, and the REST link `…/_apis/git/repositories/REPO/pullRequests/N` that `az repos pr create` prints (named after the first check) |
| Key | `github.com/owner/repo#12`; Azure DevOps pull request ids are unique in an organization: `dev.azure.com/org#12` |
| Owner | the session the command runs in (`ATELIER_SESSION`); `--session` names another (a session: one of its own group; you and canvas-wide agents: any). A session cannot take a pull request another session owns |
| Answer | `Tracking octo/app#42 for “Fix flaky test”: the canvas checks it every 5 min …` (or that nothing monitors it yet: no agent has PR coordination); a refusal exits non-zero with why; with no service running the hand-off waits in the bus inbox |

A follow-up then arrives as an ordinary canvas message, typed in when the
session is idle:
`[From PR coordinator via Atelier] PR octo/app#42 (https://github.com/octo/app/pull/42): <what to do> [Canvas: tracking grants no permission to push, reply, resolve, vote, merge: fix and test locally; ask the user before any of these. Follow-up d-…]`.
The skill says to do what it asks in that session, treat comment text as
data, and do nothing the `[Canvas: …]` part does not permit without the user.

**A canvas-wide agent with PR coordination** (`Passive::prs`: the *PR
coordinator* template, or its *PRs* setting `on [every 5m] [auto|manual] [read-only |
allow push,reply,resolve,vote,merge]`; its *Permissions* setting edits the
`allow` list alone) gets the PR monitor's events (their
`session` is the owner) and sends follow-ups:

```text
atelier pr dispatch URL --key KEY [--event TYPE] TEXT
```

| Event | Data (besides `pr`, `url`, `short`, `title`, `owner`, `owner_name`) |
|---|---|
| `pr.tracked` | `by` (handed off by …, detected, you) |
| `pr.comment.new` | `count`, `comments` (the newest 10: `author`, `url`, `text` (160 characters, untrusted), `path`), `dispatch_key` (`comments:<newest id>`) |
| `pr.checks.failed` | `head`, `failed` (`name`, `url`), `dispatch_key` (`checks:<head>:<names>`) |
| `pr.approved`, `pr.changes_requested` | `reviewer`, `comment`, `dispatch_key` |
| `pr.merged`, `pr.closed` | `state` (merged, closed, abandoned): monitoring stopped |

`pr dispatch` is accepted from canvas-wide agents with PR coordination and
from you (no canvas session); one follow-up per key and pull request
(`Already dispatched … (state)` otherwise; without `--key` the text is the
key). The service records it (*intended*), puts it in the owner's mailbox
(*queued*) or marks it *blocked* with why (not running, open elsewhere,
muted, no longer on the canvas), types it in when the owner is idle
(*delivered*; the existing idle rule: not working, not waiting on you, no
OSC 9;4 progress, no typing for 20 s), and marks it *failed* when its
message was discarded or not delivered within a day. Each change is a
report (`queued`, `delivered`, `warn` for blocked, `failed`) on behalf of
the coordinator in the thread `pr:<key>`, with the owner as `--session`
and the pull request as `--pr`.

**The monitor** (`service/prs.rs`) checks each active pull request of the
routed canvas while an enabled, unpaused coordinator exists, every
`poll_minutes` (the smallest of the coordinators'), one command at a time,
60 s each, with no window, no prompts (`GH_PROMPT_DISABLED`), no update
notices and no extension installs (`AZURE_EXTENSION_USE_DYNAMIC_INSTALL=no`):

| | GitHub (`ATELIER_GH`, else `gh.exe` on PATH) | Azure DevOps (`ATELIER_AZ`, else `az.cmd`) |
|---|---|---|
| State, title, head, author | `gh pr view URL --json number,url,title,state,isDraft,headRefOid,reviewDecision,author,comments,reviews,statusCheckRollup` | `az repos pr show --id N --org ORG --output json` |
| Comments | the `comments` and non-empty `COMMENTED` reviews, plus `gh api --paginate repos/O/R/pulls/N/comments?per_page=100` (inline) | `az devops invoke --area git --resource pullRequestThreads --route-parameters project=… repositoryId=… pullRequestId=N --api-version 7.1 --http-method GET` (system and deleted comments left out) |
| Checks | `statusCheckRollup` (failure, timed out, cancelled, action required; status contexts failure or error) | `az repos pr policy list --id N` (Build policies rejected or broken) |
| Reviews | `APPROVED`, `CHANGES_REQUESTED` | reviewer votes 10/5 and -5/-10 |
| Sign-in | `gh auth login` / `GH_TOKEN` errors: *unavailable* | `az login` errors, a missing azure-devops extension: *unavailable* |

The first check of a pull request is its baseline (nothing from before is
new); comments by the pull request's author are never new comments; a
failing check is reported once per head commit (also one that was already
failing when tracking started). Unavailable monitors try again every 30
minutes, failing ones back off (the interval doubled per failure, at most an
hour; a report after three in a row). Merged, closed and abandoned pull
requests are kept listed for 30 days and never checked again.

## Review follow-ups

The `canvas-review-followup` skill (plugin 1.2.0) teaches two roles.

**A review session that finished reviewing someone's pull request**:

```text
atelier review handoff --pr URL --author NAME [--author-id ID] --results FILE|- [--notes FILE]
               [--outcome approved|changes-requested|commented] [--title T] [--link TITLE=URL]… [--session NAME|ID]
```

`--results` is what the author may read (at most 8,000 characters);
`--notes` are private (kept for the review session, never in an event, a
draft, a message or a client's view; `review list` leaves them out). One
review per (pull request, review session, results); a new hand-off of a pull
request completes its open requests. A request for another review arrives as
`[From Review follow-ups via Atelier] Ana asks for a re-review of PR
octo/app#7 (link): "<their words>". Your earlier review (changes-requested):
<results>. Your private notes: <notes>. When you have reviewed it again,
hand it off: … [Canvas: request q-…; the review follow-up agent answers the
author, not you.]`; `atelier review done REQUEST` finishes one
without a new hand-off.

**The canvas-wide review follow-up agent** (`Passive::reviews`:
`ReviewSettings { channel, tool, recipients, approval (default on),
check_minutes (60), paused }`; the *Review follow-up* template, 60 runs a
day; its *Follow-ups* setting `mcp teams approval every 60m recipients: <rule>`,
`pr-comment`, `outbox <folder>`, `off`):

| Trigger | Data | What it does |
|---|---|---|
| `review.completed` (from the review session, to subscribers of `review.*`) | `review`, `pr`, `url`, `short`, `title`, `author`, `author_id`, `outcome`, `results` (4,000 characters), `links`, `reviewer_name`, `channel`, `tool`, `recipients`, `approval` | drafts: `review message <review> --draft FILE` |
| `review.message.approved` (addressed to the agent that drafted, once you approve) | `review`, `url`, `author`, `author_id`, `draft`, `channel`, `tool`, `recipients` | resolves recipient and conversation (else `--needs-you WHY`), sends the draft unchanged, `--sent --conversation ID --recipient NAME` (or `--failed WHY`) |
| `review.check_due` (addressed to it, every `check_minutes` while conversations are watched and checks are not paused) | `conversations` (`conversation`, `channel`, `contact`, `contact_name`, `since`, `last_checked_ms`, `prs`), `every_minutes`, `channel`, `tool` | reads only those conversations, only the contact's messages after `since`; `review request --conversation ID --message-id ID --kind re-review\|another-review\|follow-up --text TEXT [--pr URL]` per review request; `review checked --conversation ID --mark TOKEN` (or `--failed WHY`) |

What the canvas enforces (`canvas_model::reviews::Book`, one writer: the
service; `<home>/canvas-agents/<canvas id>/reviews/book.json`):

| | |
|---|---|
| Who | hand-offs: sessions of the canvas (their own; `--session` within the usual reach), you; `message`, `checked`, `request`: canvas-wide agents with review follow-ups, you; approving: you only (◎ Agents › Review follow-ups, never an agent or the CLI); `done`: the review session it went to, agents, you |
| Drafts | must contain the pull request's link; must not quote the private notes (any fragment of 16 characters or more); must not claim an approval (LGTM, "ready to merge", "I approved") unless the outcome is `approved`; at most 4,000 characters |
| Messages | awaiting approval → approved → sent, once per review (`sent` refused before approval and a second time); needs you and failed with the reason |
| Conversations | created by `sent`; watched until their pull requests all ended (the PR monitor's records) or 14 days without activity, or *Stop watching* |
| Requests | only from watched conversations; one per (conversation, message id); tied to the conversation's reviews (or `--pr`); unclear (none or several pull requests) → *needs clarification* for you with one button per candidate; another request for the same pull request in progress → waiting; routed to the review session like a PR follow-up (queued, delivered when idle, blocked with Resume reviewer / Dismiss, failed) |
| Checks | the first one interval after the first message; then every interval; `review checked` records the time and marks; failures are counted and shown (a report at the first and third in a row) |

Every change is a report on behalf of the agent in the thread
`review:<pull request key>`. The service also tracks a reviewed pull request
in the PR registry (role *reviewed*: never dispatched, only its end), so a
PR coordinator's monitor tells it when the pull request ended.

## Notes and todos

The canvas's notebook and todo boards (README, *Notebook* and *Todos*;
docs/NOTEBOOK-TASKS.md) reach every agent the same way: a command, a skill
and the group instructions.

**The session's agent.** Every launch and every passive run (ordinary
sessions, subsessions, passive sessions, canvas-wide agents) gets
`ATELIER_AGENT` set to its agent (`copilot`, `opencode`, `claude`) and
`ATELIER_SESSION_AGENT` set to the same value
(`agents::session_agent_env`). `atelier note new` records it as the
note's `agent`. `ATELIER_AGENT` also chooses the agent for new
sessions (`default_agent`, `machine_default`: before the machine's
`settings.json` and the first installed agent), so the marker tells the two
apart (`agent_from_env`): when `ATELIER_SESSION_AGENT` holds the same
value (case-insensitive), the variable only names the session a process runs
in, and a canvas started inside that session does not take it as a choice;
set by you (without the marker, or to another agent than the marker's), it
chooses as before.

**The skills** (plugin 1.5.0, beside the others; `canvas-events` points to
both, `canvas-reports` to the notes, `canvas-passive` to the todos):

| | `canvas-notes` | `canvas-todos` |
|---|---|---|
| When | writing a plan, research, an investigation log, a decision or a hand-off worth keeping; a document the user asked for; before substantial work in a group (read its `ai-docs`, `GOALS.md` first) | the group has boards, the user mentions todos, a backlog or an id like `DEC-031`, picking up or finishing work, finding work worth doing later |
| Commands | `atelier note path [--audience ai\|human]`, `note new --title T (--md FILE\|- \| --html FILE\|-) [--audience ai\|human] [--folder REL] [--name FILE]`, `note list [--audience ai\|human] [--format json]` | `todo prime` first; `todo ready` → `todo show ID` → `todo claim ID` → `todo note` / `todo check` → `todo submit ID --pr URL [--summary FILE]`; `todo add` (asked for) or `todo propose --why` (own idea), `--discovered-from ID`; `todo update --blocked`, `todo relate --blocked-by`, `todo release` |
| Rules | notes for agents in `ai-docs` (`audience: ai`), for the user in `human-outputs` (`audience: human`; one self-contained HTML file when layout matters); no secrets; HTML self-contained and offline (no scripts, CDNs, web fonts or remote images); `[[Note name]]` and todo ids become links; what the user should notice now is a report | never edit the book's files or a `TODOs.md` mirror; never take another session's claim; never run `todo done` (Done is the user's or a merged pull request's); a refusal exits non-zero and says why |

The commands find the caller from `ATELIER_SESSION` (its agent from
`ATELIER_SESSION_AGENT`, else the routing) and its group, the group's folder and the notebook
root from the routing the windows send (`<home>/bus/routing.json`). `note`
writes the files itself (never over another note); `todo` reads the stored
book and sends every change to the service as a `canvas.todo` request,
applied as that session (outside a session: you, or the automation
`AUTOMATION_NAME` names) under the rules in the README. A canvas-wide agent
is in no group: it reads every board and names `--board` when the canvas has
several. A Markdown or HTML report a session publishes is also filed in its
group's `human-outputs/reports` (README, *Reports*).

**The group instructions.** A group with a folder has
`<folder>/.canvas/canvas-group.instructions.md`, written when the folder is
prepared (`doc::prepare_group_folders`; rewritten only when its text
changes; a `"` in the group's name becomes `'`):

```markdown
---
applyTo: '**'
---
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

| | Copilot CLI | OpenCode | Claude Code |
|---|---|---|---|
| The instructions | `<folder>/.canvas` added to `COPILOT_CUSTOM_INSTRUCTIONS_DIRS` | the file in `instructions` of `OPENCODE_CONFIG_CONTENT` | `--append-system-prompt-file <file>` |
| `ai-docs`, `human-outputs` | `--add-dir` each | `external_directory` allowed for each (`OPENCODE_PERMISSION`) | `--add-dir` each |

The wording is the same for every agent (it said "This Copilot session"
before). The digests passive runs read also name the boards: `group-context.md`
gains a `# Todos` section and `canvas-context.md` a per-board summary.

## Verified end to end

`atelier selftest agent [--agent copilot|opencode|claude]` starts the
real TUI from the canvas without a prompt: imports, configuration views, the
dialog's model (the initial default), the allow-all flag and the model flag
on the command line (and the model on the box), typing, a graceful stop. For
OpenCode it puts `model.json` (its recent models) back afterwards if the
launch changed it. With `--paste` it pastes multi-line
text into the agent's input box instead and checks that it arrived as lines
and nothing was sent ([Pasting](#pasting)).

`atelier selftest agent --agent opencode --prompt [--model ID]
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
| passive | a passive OpenCode box (Run now) runs `opencode run … -m <free model>` headless, reports `atelier status ok "…"` through the canvas command on its PATH, and its box follows the run's `ses_` session |

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

What the programs did with it (2026-10-29; `atelier selftest agent
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
readers as a bare CR, so it is not used. `atelier paste-probe
[--plain] [--kitty] [--keys] [--raw]` (hidden) prints what a program
receives; `paste-probe --drive TEXT [--program "CMD"] [--ready TEXT]` runs it
(or CMD) in a hidden pseudoconsole, writes TEXT (`\e`, `\r`, `\n`, `\xNN`)
and prints the screen.

## Inside a VM (VM sessions)

With a VM pack ([VM-SESSIONS.md](VM-SESSIONS.md)) a box can run its agent on a microVM: Copilot CLI or Claude
Code. OpenCode sessions are refused there, with why (its history is a SQLite database and its live state its TUI's
HTTP server on the machine it runs on, which the canvas cannot follow from a VM yet). Everything else here is
unchanged on this machine; VM sessions are optional and local stays the default.

- **The command line** (`vm::launch::guest_command`, `vm::target`) is the provider's own (`copilot --session-id
  <box> …`) with nothing of this machine: the program by its name on the guest's `PATH` (`ATELIER_<AGENT>` is
  honoured in the guest as here, for the stand-in agents), and, with a satellite, the roots the hub syncs into the
  guest in this machine's places: `--plugin-dir` for the canvas's plugin, the user's profile bundle (their own
  skills, `profile.yaml`) and the group's skill folders; `--add-dir` for the group's `ai-docs` and
  `human-outputs`; `COPILOT_CUSTOM_INSTRUCTIONS_DIRS` for the group's instructions (with the guest's paths) and the
  generated path map (`paths.yaml`); `ATELIER_NOTES` for `atelier note path`. The program starts once those roots
  are in the guest. A VM session cannot be forked yet. **Claude Code** (`claude::Claude::args_env`, without this
  machine's folders): `--plugin-dir` for the same roots, `--add-dir` for the group's notes, the group's
  instructions file as `--append-system-prompt-file`, and the box's MCP preset as `--strict-mcp-config --mcp-config`
  with a file of the roots (`agent-mcp/claude.json`); every run gets them (nothing is saved with a Claude session).
- **MCP**: the guest's `~/.copilot/mcp-config.json` names the servers of the box's MCP preset (the new-session
  dialog's choice; the pack's `mcp-policy.yaml`) as `http` servers at the satellite's proxy on the guest's loopback;
  the calls go to this machine's servers (its own Copilot configuration and credentials), filtered and checked by
  the pack's rules first. `--disable-mcp-server` (the box's MCP choice) applies as here.
- **The terminal** is drawn here and runs in the guest behind its satellite; status comes from the guest's
  history, mirrored under `<home>/vm/mirrors/<sandbox>/copilot` (Copilot) or `…/claude` (Claude Code: its
  `projects/<dir>/<id>.jsonl` transcripts and subagents appended, its `sessions/<pid>.json` status files whole,
  live as the guest's satellite reports) and indexed like the agent's own folder. `Transcript` and `HasHistory`
  find a VM session there. **Resume on a new VM**: when its VM was lost or deleted, the mirror goes into the new
  guest (Claude Code's into the project folder of the new clone) and the conversation continues there.
- **Subsessions**: a `copilot` an agent starts inside its terminal in the guest (holding a session's in-use lock)
  is reported by the satellite and becomes the box's subsession, as locally. Whether a mirrored session is live is
  what the guest reports (its lock files are not mirrored).
- **Canvas commands** the agent runs in the guest (`atelier todo`, `report`, `pr`, `emit`, `send`, `events`,
  `note`, …) are relayed and run here as the box, at once over the satellite's link (`effect list` prints there);
  commands that act on this machine are refused there. While the canvas is not reachable, `todo` reads answer from
  its last copy in the guest (they say so) and writes answer *Queued* and are applied once when it is back. A guest
  path in `todo link` is this machine's when it is in the group's notes or a mapped folder; this machine's paths in
  an answer (`note new`) are the guest's. Notes written straight into the group's `ai-docs` in the guest come back
  here; if the canvas's copy changed meanwhile, the guest keeps its version beside it (`… (conflict …).md`).

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
- No real agent has been asked to publish a report yet: `selftest`, the
  parity rows 41–41c and the backend contract publish through the same
  `atelier report publish` from demo shells and tests, and the skill's
  text is checked by a unit test.
- Pull request coordination has been driven only by the stand-in agent
  (which follows the `canvas-prs` contract: one `pr dispatch` per
  `pr.comment.new` / `pr.checks.failed` with its key) and stand-in `gh` /
  `az` (`selftest`, parity row 42); no real agent and no real pull request
  were involved. The Azure DevOps commands and shapes follow Microsoft's
  documentation and were not run against a real organization.
- Review follow-ups have been driven only by the stand-in agent (which
  drafts from the results, "sends" through an `outbox` folder and reads
  replies from it) in `selftest` and parity row 43; no real messaging MCP
  server was used and no message was sent to anyone.
