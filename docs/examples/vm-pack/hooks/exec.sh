#!/bin/sh
# Example: run a command in the guest and return its output.
set -eu
python3 -c '
import base64, json, shlex, subprocess, sys
r = json.load(sys.stdin)
cmd = " ".join(shlex.quote(a) for a in r["argv"])
if r.get("stdin"):
    cmd = "echo %s | base64 -d | %s" % (r["stdin"], cmd)
p = subprocess.run(["aca", "sandbox", "exec", "--id", r["sandbox"], "-c", cmd], capture_output=True)
print(json.dumps({"ok": True, "code": p.returncode, "stdout": base64.b64encode(p.stdout).decode(), "stderr": p.stderr.decode(errors="replace")[-2000:]}))'
