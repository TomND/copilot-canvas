# VM sessions: prompts for parallel workers

Self-contained prompts for packages of [VM-SESSIONS-PLAN.md](VM-SESSIONS-PLAN.md) §6.3 that touch only
files of their own. The lead integrates each worker's sub-branch into `vm-sessions` and wires it into the
hot files (`service/mod.rs`, `server.rs`, `term.rs`, `main.rs`, …), which workers never edit. Status and
progress: [VM-STATUS.md](VM-STATUS.md).

Rules every worker follows (also in AGENTS.md): Rust 2024, no rustfmt (match the style, long lines are
fine), `cargo clippy -p canvas-agents --no-default-features --all-targets -- -D warnings` clean, unit tests
next to the code, temporary folders only (never `~/.atelier`, `~/.copilot-canvas`, the owner's
LaunchAgents, Keychain or crontab), **never call Azure or any real service** (tests use local stand-ins
only), no pack values in the repository (examples use `contoso`/`example.com`), no emojis. On this Mac:
`source ~/.cargo/env; export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=line-tables-only`, a target folder
of your own outside the repository, `--no-default-features`, check `df -h ~` (keep 4 GB free). Commit on
your sub-branch in small steps (subjects say what changed and why, as `git log` does); do not push to
`vm-sessions` or `main`.

---

## Worker P-aca: `vm::aca`, the Azure Container Apps Sandboxes provider (Phase 1, VM-12)

Branch: `vm-aca` from `vm-sessions` (at or after the commit that adds `crates/canvas-agents/src/vm/provider.rs`).

**Goal.** Implement `canvas_agents::vm::provider::Provider` for Azure Container Apps Sandboxes' data plane,
REST only (no CLI on the hot path), tested only against an in-process HTTP stand-in. Files you own:
`crates/canvas-agents/src/vm/aca/{mod.rs,rest.rs,model.rs,token.rs}` (new). You may add to
`crates/canvas-agents/Cargo.toml`: `reqwest` with the same per-OS TLS features `crates/canvas-push/Cargo.toml`
uses (blocking, native-tls off Linux, rustls with ring on Linux). Do not edit any other file; the lead adds
`pub mod aca;` to `vm/mod.rs` (add it locally to compile, and say so in your hand-off).

**The API** (from the public Python SDK `azure-containerapps-sandbox` 0.1.0b4; plan Appendix A): base
`https://management.<region>.azuredevcompute.io` (or `Pack::aca.endpoint`), `?api-version=` the pack's
`aca.api_version` (default `2026-02-01-preview`), bearer token. Group path
`G = /subscriptions/{sub}/resourceGroups/{rg}/sandboxGroups/{group}`, sandbox `S = G/sandboxes/{id}`.
- create `PUT G/sandboxes` body `{ "sourcesRef": { "diskImage": { "id": … } | { "name": "ubuntu", "isPublic": true } }, "resources": { "cpu": "2000m", "memory": "4096Mi", "disk": "20Gi" }, "lifecycle": { "autoSuspendPolicy": { "enabled": false, "interval": 300, "mode": "Memory" } }, "labels": {…}, "environment": {…}, "egressPolicy": {…} }` → sandbox `{ "id", "state", "labels" }`; then poll `GET S` until `Running` (bounded: 180 s; `Failed`/`Deleting` end it).
  **Auto-suspend is always off** (the platform's idle detector ignores outbound traffic; the canvas suspends itself); never set an auto-delete policy.
- list `GET G/sandboxes?labels=k=v,k2=v2` → `{ "value": [...], "nextLink" }` or a bare list (follow `nextLink` only when it starts with the same endpoint).
- `GET S`, `DELETE S` (200/202/204; a 404 on delete is success).
- suspend `POST S/stop`, resume `POST S/resume` (then poll for `Stopped`|`Suspended` / `Running`); a 409 on resume of a running sandbox is success.
- exec `POST S/executeShellCommand` `{ "command": <shell-quoted argv>, "workingDirectory"? }` → `{ "exitCode", "stdout", "stderr" }`. The API takes a shell string: quote every argv element POSIX-style (`'…'` with `'\''`), never pass text unquoted; stdin is given as `printf %s <base64> | base64 -d | …` when not empty.
- files `PUT S/files?path=<abs>` (raw bytes), `GET S/files?path=<abs>` (bounded by `max`).
- egress `POST S/egresspolicy` `{ "defaultAction": "Deny", "rules": [ { "name", "match": { "host", "path", "methods" }, "action": { "type": "Allow"|"Deny"|"Transform", "headers": [ { "operation": "Set", "name", "valueRef": { "secretRef": { "secretId", "secretKey", "format" } } } ] } } ] }`; `GET S/egress-decisions`.
- images: `PUT G/diskimages` `{ "image": { "base": … }, "labels": { "name": … } }`, `POST S/commit` `{ "labels": { "name": … } }` → `{ "diskImage": {…} }`, `GET G/diskimages`, `DELETE G/diskimages/{id}`.
- secrets: `PUT G/secrets/{id}` `{ "values": { key: value } }` (the value only in this body; never logged).
- Every path segment you build from data is checked (no `/`, `\`, `..`, `?`, `#`, NUL).

**Token** (`token.rs`): the pack's `auth.command` if set (run as the user, stdout is the token), else
`az account get-access-token --resource <auth.audience> -o json` (audience default `https://dynamicsessions.io`,
a pack setting). Cache until 5 minutes before `expiresOn`; refresh on a background thread so a launch never
waits for the CLI unless the cache is empty. The token never appears in errors, logs or `Debug`.
Make the command injectable for tests (a closure or a trait), so tests never run `az`.

**Retries.** 429 and 5xx: retry with jitter, honour `Retry-After` (seconds), at most 4 tries and 60 s in all;
other 4xx are errors carrying the status and the response's `error.code`/`error.message` (first line, 200
characters), never the token or the request body.

**Tests** (all in-process, no network): an HTTP/1.1 stand-in on `127.0.0.1:0` (`std::net::TcpListener`, a
thread, scripted responses, recording every request). Cover: create → poll → Running with the exact JSON
body (golden), list with labels and a `nextLink`, exec quoting (an argv with spaces, quotes, `$(…)`, `;`
reaches the stand-in correctly quoted), stdin through base64, 429 with `Retry-After: 1` then success, a 5xx
burst, 409 on resume, 404 on delete, malformed JSON, egress policy body, secret body, the token cached (one
command call for many requests) and refreshed when near expiry, the token absent from every error string.

**Hand-off.** `pub struct Aca` with `Aca::new(pack: &Pack, token: Box<dyn TokenSource>) -> Result<Aca>` and
`impl Provider for Aca`; a list of what is not covered; the commit range.

---

## Worker P-local: `vm::local` as a full provider (Phase 1, VM-13)

Branch: `vm-local` from `vm-sessions` (at or after the commit that adds `vm/provider.rs`).

**Goal.** Make `canvas_agents::vm::local` (today: sandboxes as folders for `vm-hook-standin`) a complete
`Provider` for tests and CI on Windows, macOS and Linux, including memory-suspend semantics. Files you own:
`crates/canvas-agents/src/vm/local.rs` (extend it; keep `standin`, `standin_run`, `Local::claim/exec/env`
working as they are: `atelier selftest vm` uses them), `crates/canvas-sys/src/process/{mod,unix,windows,macos,linux}.rs`
(only to add `suspend_tree` / `resume_tree`). Do not edit other files.

**Behaviour.**
- `impl Provider for Local`: create (a folder per sandbox with `home/` and `work/`, labels and state in
  `state.json`, an "image" is a folder under `<root>/images/<id>/` copied into `work/` at create), get, list
  by labels, exec (as `Local::exec`), put/get file (inside the sandbox folder only: refuse paths that leave it,
  links included), delete (kill the sandbox's processes, then remove the folder), egress (store the policy in
  `egress.json` for the egress stand-in of Phase 3), egress decisions (read `egress-decisions.jsonl` if present),
  image build/commit/delete/list (folders), secret_set (to `<root>/secrets/<name>.json`, 0600 on Unix, **never
  under a sandbox folder**).
- **Suspend/resume**: the sandbox's processes are recorded (`processes.json`: pid and start time of every
  process started through `exec`/`command`, and their descendants each time); suspend freezes the whole tree
  with `canvas_sys::process::suspend_tree` (Unix: `SIGSTOP` to each process of `process_tree`, the root last;
  Windows: `NtSuspendProcess` per process through `ntdll`, or `SuspendThread` over a Toolhelp thread snapshot),
  resume thaws it (`SIGCONT`, root first). State `Suspended`/`Running` in `state.json`. Pids are checked
  against their start time before every signal (never signal a recycled pid).
- Failure injection for tests: `ATELIER_VM_TEST_FAIL=create:1,resume:2` (fail the Nth call of that kind),
  `ATELIER_VM_TEST_LATENCY_MS=…` (sleep before each call). Read only by `vm::local`.

**Tests.** A child process that prints a counter every 50 ms: no output while suspended, output again after
resume (all three OSes); a recycled pid is not signalled; put_file refuses `../x`, an absolute path and a link
leading out; delete leaves no process and no folder; secrets never under a sandbox folder or in `exec`'s
environment; list by labels; the injection knobs. `cargo test -p canvas-agents --lib vm::local` and
`cargo test -p canvas-sys` pass; `cargo xwin clippy --target x86_64-pc-windows-msvc -p canvas-agents -p canvas-sys --no-default-features --all-targets -- -D warnings` is clean.

**Hand-off.** What `Provider` calls are covered by tests, the commit range.

---

## Worker P-egress: the egress engine and its stand-in proxy (Phase 3, VM-36)

Branch: `vm-egress` from `vm-sessions` (at or after the commit that adds `crates/canvas-agents/src/vm/egress/mod.rs`).

**Goal.** A VM session's guest reaches only what its pack allows: the pack's egress profile, rendered for the
box at its claim, decides every outbound request (host, path, method), and `Transform` rules add headers whose
values (tokens) the egress point holds and the guest never sees. `vm::aca` sends the rendered policy to the
platform, which enforces it there (already done: `Provider::set_egress`, `SandboxSpec::egress`). You build the
engine and the stand-in proxy that enforces the same policy for `vm::local` sandboxes in tests and CI. Files you
own: everything under `crates/canvas-agents/src/vm/egress/` (`mod.rs` exists with the interface below; keep its
public items and signatures, replace the bodies; add `render.rs`, `rules.rs`, `proxy.rs`, `standin.rs` or as you
see fit, declared in `mod.rs`). Do not edit other files (the secrets refresher, `vm/secrets.rs`, is done; the lead
wires the stand-in into `vm::local`, `main.rs` and the launcher).

**Interfaces you use** (read them; do not change them):
- `vm::provider::{EgressPolicy, EgressRule, HeaderFromSecret, EgressDecision, Secret}`: the policy (`default_action`
  `Allow`|`Deny`, rules with `name`, `host`, `path`, `methods`, `action` `Allow`|`Deny`|`Transform`, `headers` from
  secrets: header name, secret name, key, `format` with `{}` for the value); `Secret` is zeroized on drop and never
  printed.
- `vm::pack::Pack`: `pack.egress` (profile name → the YAML text of `egress/<name>.yaml`), `pack.secrets` (each
  secret's `name` and `key`), `pack.tunnel.url`. The format (`docs/examples/vm-pack/egress/forge.yaml`):
  `default: deny|allow`, `rules:` of `host`, optional `path` (a prefix or a `*` glob), optional `methods`, `action`
  `allow|deny|transform`, `headers:` of `name` and `value` (`value` must contain `{{secret:NAME}}`, e.g.
  `"Bearer {{secret:forge-token}}"` → secret `forge-token`, its `key`, format `Bearer {}`).
- `vm::local::Local` (the stand-in's data): `egress(id) -> Result<Option<EgressPolicy>>` (the sandbox's
  `egress.json`), `secret(name, key) -> Result<Option<Secret>>` (held outside every sandbox),
  `log_egress(id, &EgressDecision)` (what `Provider::egress_decisions` reads), `command(id, argv, cwd, env)` +
  `spawn(id, &mut cmd)` to run a "guest" process in a sandbox for tests.
- `vm::satellite::dial::{parse, proxy_for}`: how the guest's satellite uses `HTTPS_PROXY`/`HTTP_PROXY` (`CONNECT`
  for both `ws://` and `wss://`) and `NO_PROXY`.

**The interface you implement** (`vm/egress/mod.rs`):
- `render(pack, name, &Vars) -> Result<EgressPolicy>`: placeholders `{{box.branch}}`, `{{box.repo}}`, `{{box.id}}`,
  `{{sandbox}}`, `{{hub.host}}` in hosts and paths; `{{secret:NAME}}` only in header values (anything else there is
  an error: no literal token may reach a policy); an unknown placeholder or secret is an error naming the profile
  and rule, never a value.
- `decide(policy, &Request) -> Decision`: first matching rule wins (host exact or `*.example.com`, case-insensitive;
  port ignored unless the rule names one; path prefix or glob; methods if given); none: the default action;
  `CONNECT` matches on host only (a rule with a path or methods never allows a `CONNECT`).
- `serve(listener, policy, secrets, recorder)`: an HTTP/1.1 forward proxy. Absolute-form requests (and `Upgrade:
  websocket` through them) are decided on host, path and method; `Transform` headers are set (replacing any the
  guest sent) from `Secrets::value`; `CONNECT` is decided on host only and then tunnels bytes (no injection
  possible: say so in a comment). A refusal answers `403` with a short text naming the rule or "denied by
  default", and is recorded (`Recorder::record`, `allowed: false`); allowed requests are recorded too. Bounded:
  header size, a connect timeout, idle timeouts; one thread per connection is fine. Never log a header value.
- A stand-in entry the lead calls from a hidden `atelier egress-standin --root ROOT --sandbox ID` subcommand, e.g.
  `pub fn standin(root: &Path, sandbox: &str, port_file: &Path) -> Result<()>`: binds `127.0.0.1:0`, writes the
  port, serves with the policy read from `Local::egress` for every request, secrets from `Local::secret`, decisions
  to `Local::log_egress`.

**Tests** (in-process, loopback only, never a real host; ports `127.0.0.1:0`): the rules table (first match,
default deny and allow, `*.` hosts, path prefix and glob, methods, `CONNECT` with a path rule refused, case);
`render` of the example's `forge.yaml` and of placeholders, every error case; the proxy against a local upstream
that echoes the request it got: allowed `GET`, refused host and method (403, recorded), `Transform` injects
`Authorization` and replaces a guest-sent one, `CONNECT` allowed and refused, an `Upgrade` passes; a policy
changed between two requests applies to the second; secrets never in a recorded decision, an error, a log line or
`Debug`. One "guest" test: a process started in a `vm::local` sandbox (the test binary itself as a helper, as
`vm/local.rs`'s tests do) with `HTTP_PROXY` set to the stand-in sends an absolute-form request: refused for a
forbidden host, and for an allowed one the upstream sees the injected header while the sandbox's environment and
files never contain the value.

**Commands.** `cargo test -p canvas-agents --no-default-features --lib vm::egress`; `cargo clippy -p canvas-agents
--no-default-features --all-targets -- -D warnings`; Windows: `export PATH=/opt/homebrew/opt/llvm/bin:$PATH; cargo
xwin clippy --target x86_64-pc-windows-msvc -p canvas-agents --no-default-features --all-targets -- -D warnings`;
Linux (optional): `source ~/.local/zig-0.17.0/wrap/env.sh; cargo clippy --target x86_64-unknown-linux-gnu -p
canvas-agents --no-default-features --all-targets -- -D warnings`.

**Hand-off.** The final API (`render`, `decide`, `serve`, the stand-in entry), what the lead must wire (the hidden
subcommand; `vm::local` starting one stand-in per sandbox outside the sandbox's processes and setting
`HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` in `Local::env`; the launcher rendering the repository's profile at a claim
into `SandboxSpec::egress` / `set_egress`), what is not covered, the commit range.

---

## Worker P-mcp: the MCP gateway and its stand-in server (Phase 3, VM-37)

Branch: `vm-mcp` from `vm-sessions` (at or after the commit that adds `crates/canvas-agents/src/vm/mcp/mod.rs`).

**Goal.** An agent in a VM session uses the hub's MCP servers (for example a forge's) without their credentials
ever entering the guest, and only as far as the pack's `mcp-policy.yaml` allows. In the guest, the satellite
serves an MCP endpoint on loopback; each agent session there becomes a stream on the satellite's link; on the hub
the gateway checks the policy, starts the hub's server for that session and relays JSON-RPC, filtering and
refusing. Files you own: everything under `crates/canvas-agents/src/vm/mcp/` (`mod.rs` exists with the interface
below; keep its public items and signatures, replace the bodies; add `gateway.rs`, `policy.rs`, `jsonrpc.rs`,
`server.rs`, `standin.rs` or as you see fit) and `crates/canvas-agents/src/vm/satellite/mcp_proxy.rs` (exists, the
same rule). Do not edit other files: the link and the satellite already call your code (`vm/link.rs`,
`vm/satellite/mod.rs`); the lead writes the guests' MCP configuration (VM-38), adds the hidden `atelier mcp-standin`
subcommand and the `selftest vm` steps.

**The link** (done; read `canvas_common::sat`): `SatUp::Mcp { stream, kind, bytes }` guest → hub and
`SatDown::Mcp { … }` hub → guest; kinds `MCP_OPEN` (bytes: JSON `McpOpen { term, server }`), `MCP_MESSAGE` (one
JSON-RPC message, UTF-8 JSON), `MCP_CLOSE` (either side ends the stream), `MCP_ERROR` (hub: refused, JSON
`{"message"}`, followed by `MCP_CLOSE`). Stream ids are the satellite's, unique on its link; a dropped link ends
every stream on both sides (MCP clients then start a new session). Payloads up to `SAT_FILE_CHUNK`; messages travel
on the link's second lane, after terminal I/O.
- Hub: `vm::link::Links` calls `Gateway::up(sandbox, stream, kind, bytes)` for every `SatUp::Mcp` of a satellite
  it authenticated for `sandbox` (do not trust `McpOpen::term`: check it is a box of that sandbox, see below),
  `Gateway::link_ended(sandbox)`, `Gateway::session_ended(term)` when a box's program exits; you answer through
  `Down::send(sandbox, stream, kind, bytes)` (`false`: the link is down).
- Guest: the satellite creates one `McpProxy::start(0)` for its lifetime, reports `port()` in
  `HelloInfo::mcp_port`, calls `attach(Some(sender))` for each link (send your `SatUp::Mcp` there) and
  `attach(None)` + `link_lost()` when it drops, and `down(stream, kind, bytes)` for each `SatDown::Mcp`.

**Hub side** (`vm/mcp/`):
- The box: `vm::launch::load(home, term)` gives its `Intent` (`sandbox` must equal the stream's sandbox, else refuse;
  `placement.branch` (else `canvas_model::vm::branch_for_session(term)`), `placement.repo`, `placement.mcp` = the
  pack's preset name, empty: no servers). The pack: `vm::pack::configured(home)` + `vm::pack::load` →
  `pack.mcp: McpPolicy { presets: name → servers, servers: name → McpServerPolicy { mode: per_session|shared,
  protocol, rules: [McpRule { action: allow|deny, tool, when: arg → value with {{box.branch}} / {{box.repo}} }] } }`.
  A server is allowed when the box's preset lists it and the pack has a policy for it; default deny everywhere.
- Servers: `ServerSource::spec(name) -> Option<ServerSpec>` (`Stdio { command, args, env }` or `Http { url, headers
  }`). Write the real source over the hub's Copilot configuration (`<COPILOT_HOME>/mcp-config.json`, `mcpServers`:
  `type` `local`/`stdio` with `command`, `args`, `env`; `http`/`sse` with `url`, `headers`), taking the file's path
  (tests use temporary files, never the real `~/.copilot`); the lead switches `Links` from `NoServers` to it.
- Per (box session, server) one server process, started lazily at the first stream's `initialize`, ended with the
  session (`session_ended`), the stream's close for `per_session`, an idle 10 minutes, or the link's end; `shared`
  keeps one per server. The protocol version is pinned when the policy names one.
- Policy: `tools/list` answers filtered to allowed tools; every `tools/call` checked (an unknown tool or argument,
  a `when` mismatch → a JSON-RPC error to the guest, the server never sees it); notifications and cancellations
  passed for live requests; server-initiated requests (`roots/list`, `sampling/createMessage`,
  `elicitation/create`) refused unless the policy allows them. Refusals recorded in `<home>/vm/mcp/refusals.jsonl`
  (time, sandbox, box, server, tool, rule; never argument values that look like secrets).

**Guest side** (`vm/satellite/mcp_proxy.rs`): MCP Streamable HTTP on `127.0.0.1:<port>`, path
`/mcp/<term>/<server>`: `POST` JSON-RPC (answer JSON, or `text/event-stream` when the answer streams), `GET` the
server-to-client stream (SSE), `DELETE` ends the session; `Mcp-Session-Id` per session (one stream each). Refuse
other hosts in `Host`/`Origin` (DNS-rebinding), bound sizes and time. A link down: answer `503`, and `404` for
sessions it ended.

**The stand-in server** (`vm/mcp/standin.rs`, `pub fn run(input: impl BufRead, output: impl Write, log: &Path) ->
Result<()>`; the lead adds `atelier mcp-standin`): MCP on stdio; tools `list_pull_requests`,
`create_pull_request {draft, head, base, title}`, `update_pull_request`, `vote`, `add_comment`; it logs every
message it receives (one JSON line each), checks a credential in its own environment (`ATELIER_MCP_STANDIN_TOKEN`)
and says so in its answers, can send a server-initiated request and a progress notification on demand.

**Tests** (in-process or child processes of the test binary, loopback only): with the stand-in as the hub's server
and the example pack's policy: `initialize`, `tools/list` filtered to the allowed ones; a refused `vote` and
`update_pull_request` never reach the stand-in (its log); `create_pull_request` with `draft: true` and the box's
branch as `head` passes, another head is refused; a cancellation passes; a server-initiated request is refused;
one process per session and none after `session_ended`; a stream naming another sandbox's box is refused; the
credential never in the guest (not in a `Down` message, not in the proxy's answers). The proxy against a fake
link (a channel): `POST`/SSE/`GET`/`DELETE`, session ids, a dropped link answers `503`, a foreign `Origin` is
refused. JSON-RPC framing: batches refused or split as the protocol version says, ids preserved, large messages.

**Commands.** `cargo test -p canvas-agents --no-default-features --lib vm::mcp vm::satellite::mcp_proxy`; the clippy
lines of P-egress (macOS, `cargo xwin` for Windows, optional Linux).

**Hand-off.** The final API, the `ServerSource` over the hub's Copilot configuration, what the lead must wire
(the source in `Links::new`, VM-38's guest configuration pointing at `http://127.0.0.1:<mcp_port>/mcp/<term>/<server>`,
`atelier mcp-standin`, the probe's steps), what is not covered (HTTP servers on the hub, other agents), the commit
range.
