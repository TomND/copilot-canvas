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
