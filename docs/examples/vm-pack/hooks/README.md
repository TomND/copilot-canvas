# Hooks (the thin lane, `provider: hooks`)

Each hook reads one JSON object on standard input and prints one JSON object
on standard output (`{"ok": true, …}` or `{"ok": false, "error": "…"}`).
They run as you on the hub. Example scripts using the `aca` CLI:

| Hook | Input | Output |
|---|---|---|
| `claim` | `box`, `repo`, `url`, `branch`, `base`, `size`, `labels` | `sandbox` |
| `shell-command` | `sandbox`, `box`, `argv`, `env`, `cwd` | `argv`: the command the box's terminal runs (an interactive shell into the guest) |
| `exec` | `sandbox`, `argv`, `stdin` (base64) | `code`, `stdout` (base64), `stderr` |
| `suspend`, `resume`, `delete` | `sandbox` | — |
| `image` | `repo`, `url`, `base` | `image` |
