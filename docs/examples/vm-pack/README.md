# Example VM pack

A VM pack is the configuration of VM sessions for one user or company
(docs/VM-SESSIONS.md §5). This folder is an **example**: every value is a
placeholder (`contoso`, `example.com`, zero ids). Copy it **outside any
repository**, fill in your own values, point Atelier at it and check it:

```sh
cp -r docs/examples/vm-pack ~/atelier-vm-pack        # outside every git work tree
atelier config vm.pack ~/atelier-vm-pack             # or ATELIER_VM_PACK=~/atelier-vm-pack
atelier vm pack check                                # findings only, never values
```

VM sessions are optional; local stays the default. Without a pack nothing
changes; with one, *Run on* in the new-session dialog still starts on this
machine unless you pick a VM.

| File | Holds |
|---|---|
| `vm.yaml` | provider (`aca`, `hooks`, `local`), the sandbox group, sizes, pool, images, budget, suspend-after, the tunnel, git identity, the satellite binary and its SHA-256, `policy_ack` |
| `repos.yaml` | repository keys: clone URL, default branch, clone mode, image base and disk, egress profile, the local folders that hold the repository |
| `egress/*.yaml` | deny-by-default egress per profile; header values only from `{{secret:NAME}}` |
| `secrets.yaml` | how the hub refreshes group secrets: a command whose output is the value (never logged or stored) |
| `mcp-policy.yaml` | the MCP servers guests may use, presets, and default-deny rules over tools and arguments |
| `profile.yaml` | personal skills and instructions to bundle; what is always excluded |
| `paths.yaml` | host folders mapped to guest paths (`ro-snapshot`, `rw-notes`, `none`) |
| `hooks/` | external commands for the thin lane (`provider: hooks`): claim, shell-command, exec, suspend, resume, delete, image; JSON on standard input and output |
