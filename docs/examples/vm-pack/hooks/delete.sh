#!/bin/sh
set -eu
id=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["sandbox"])')
aca sandbox delete --id "$id" --yes >/dev/null
echo '{"ok": true}'
