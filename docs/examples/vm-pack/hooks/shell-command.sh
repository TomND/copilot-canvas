#!/bin/sh
# Example: the box's terminal joins a tmux session in the guest that runs the agent.
set -eu
python3 -c '
import json, shlex, sys
r = json.load(sys.stdin)
agent = " ".join(shlex.quote(a) for a in r["argv"])
print(json.dumps({"ok": True, "argv": ["aca", "sandbox", "shell", "--id", r["sandbox"], "-c", "tmux new -A -s %s %s" % (r["box"][:8], shlex.quote(agent))]}))'
