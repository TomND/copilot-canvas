#!/bin/sh
# Example: create a sandbox for a box with the aca CLI (placeholders only).
set -eu
input=$(cat)
box=$(printf '%s' "$input" | python3 -c 'import json,sys; print(json.load(sys.stdin)["box"])')
id=$(aca sandbox create --disk ubuntu --label atelier.box="$box" --output json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
printf '{"ok": true, "sandbox": "%s"}\n' "$id"
