---
name: canvas-verify
description: Verify a Copilot Canvas change before committing it, in the right order and safely on the owner's live machine (clippy, cargo test, the release build, the TUI and GUI self-tests, the GUI parity suite, golden screenshots, hardening, the terminal corpus). Use when you finish a code change, before a commit or a pull request, when asked to run the tests or check that everything still passes, when you want one test, one snapshot or a few parity rows, or when a suite failed and you need its logs and sandbox.
---

# Verifying a Copilot Canvas change

The depth (what each suite does, how to write tests) is in
`docs/DEVELOPMENT.md` section 3; this is the procedure.

## What a change needs

| Change | Run |
|---|---|
| Docs only | nothing (check links and names against the code) |
| Code | clippy and `cargo test --workspace` |
| Anything a user can see (TUI, GUI, CLI output, skills) | everything below |
| Protocol, document, stores, backend requests | everything, the `--remote loopback` runs included |
| A portable crate (`canvas-common`, `canvas-model`, `canvas-termview`, `canvas-push`, `canvas-mobile`, `canvas-mock`) | also the portable clippy line below |
| The terminal engine or the host | also `corpus check` (it is in the list anyway) |

## Shell rules (the owner's machine is in use)

Run builds below normal priority with a limited job count, in the same
PowerShell where cargo runs:

```powershell
(Get-Process -Id $PID).PriorityClass = 'BelowNormal'; $env:CARGO_BUILD_JOBS = '6'
```

Write long outputs to a file and read its end, not the whole stream:

```powershell
cargo test --workspace *> "$env:TEMP\cc-test.log"; "exit $LASTEXITCODE"
```

Never stop processes you did not start (the owner's canvas service and
agent sessions run here), never touch `~/.copilot-canvas`; every suite
below works in its own sandbox. Do not rebuild while a suite runs.

## The order (fix every failure before the next step)

```text
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                                   (about 3 min after a build)
cargo build --release                                    (cold: about 10 min)
target\release\copilot-canvas.exe selftest               (about 145 s; so are the loopback and GUI runs)
target\release\copilot-canvas.exe selftest --remote loopback
target\release\copilot-canvas.exe selftest --remote faults
target\release\copilot-canvas.exe selftest --remote paired
target\release\copilot-canvas.exe selftest --client gui
target\release\copilot-canvas.exe selftest --client gui --remote loopback
target\release\copilot-canvas.exe selftest --client gui --parity      (about 270 s)
target\release\copilot-canvas.exe selftest --client gui --golden      (about 30 s)
target\release\copilot-canvas.exe selftest --client gui --hardening   (about 32 s)
target\release\copilot-canvas.exe corpus check
```

Portable crates (CI builds them on macOS):
`cargo clippy -p canvas-common -p canvas-model -p canvas-termview -p canvas-push -p canvas-mobile -p canvas-mock --all-targets -- -D warnings`.

The `--client gui` suites open real windows on the owner's desktop: run them
**one at a time**, never two together, never in parallel with another GUI
suite. Never run `selftest agent` (it drives the real agent CLIs) unless the
user asks for it. Never run `--golden --update` without looking at every
changed PNG (skill `canvas-ui-testing`).

## A subset while working

- One crate or module: `cargo test -p canvas-agents service::todos`,
  `cargo test -p canvas-core --lib undo`.
- One view snapshot: `cargo test -p canvas-core --test view_snapshots every_status`.
- One contract test: `cargo test -p copilot-canvas --test backend_contract skill_sources`.
- Some parity rows: `$env:CANVAS_PARITY_ROWS = '41,41b'`, run `--parity`,
  then `Remove-Item Env:CANVAS_PARITY_ROWS`. Rows build on earlier ones
  (39b needs 38): add the rows a row needs. Only the full suite counts.

## Reading a failure

- `cargo test`: the panic text; a snapshot failure names the `.snap.new`
  file next to the snapshot (accept only after reading the diff).
- Self-test and parity print `artifacts kept in <root>` on failure: the
  sandbox (`%TEMP%\copilot-canvas-selftest-…` or `…-parity-…`) holds
  `gui-stderr.log` / `gui-parity.log` (and other `gui-*.log`), and the
  service log `home\service-<instance>.log`; a GUI panic also writes
  `home\crash.log`. The failing step or row prints what it waited for and the
  screen it saw. `CANVAS_GUI_SCREENS=<folder>` keeps PNG captures.
- Golden: `tests\golden\gui\<name>.actual.png` and `<name>.diff.png` (red
  pixels differ, yellow blocks failed the luminance guard, blue is masked).
- Delete a kept sandbox once you are done with it (they are yours).

## Flaky or real

Re-run the failing suite (or just its rows) once. If it fails again, it is
real: find the cause and fix it. If it passes, the first failure still says
something: usually a check that assumed timing (a fixed sleep, a state left
by an earlier row, a slow runner). Fix the wait or the cleanup in the test,
never by loosening what it checks, skipping it or raising tolerances.

A failure that only happens when you (an agent the canvas started) run the
tests can come from your environment: your variables and, under OpenCode,
Ctrl+C ignored by every child process. Confirm by running the suite from a
clean process (docs/DEVELOPMENT.md, *Pitfalls*), then make the test hermetic.

## After a full run

Update the *Last run* section of `docs/PARITY.md` (date, `PASS: N rows in T s`,
the self-test, golden and hardening results) and give the results in the
commit message, as earlier commits do (`git log`).
