#!/bin/sh
# Makes this Mac a stand-in "Cloud PC" for the iPhone app: the mock service
# (real shells, and your real OpenCode / Claude Code / Copilot for sessions
# started from the phone) on 127.0.0.1:$PORT behind a private Dev Tunnel,
# reached by the phone exactly like the Windows service: wss:// web
# forwarding with a tunnel access token, paired with a one-time code.
#
#   ios/cloud-mac.sh start     build, start the mock and the tunnel host, show the pairing QR code
#   ios/cloud-mac.sh pair      a new one-time code and QR code (restarts the mock; pairings stay)
#   ios/cloud-mac.sh status    what runs, the tunnel, the address
#   ios/cloud-mac.sh stop      stop both
#   ios/cloud-mac.sh logs      follow the mock's and the tunnel's logs
#   ios/cloud-mac.sh report FILE   publish FILE (.html, .md, else a message) to the reports inbox
#
# Needs the Dev Tunnels CLI signed in as the tunnel's owner
# (`brew install --cask devtunnel`, `devtunnel user login -g`).
#
# Notifications while the app is away: APNS_KEY=~/AuthKey_ID.p8 APNS_TEAM=TEAMID
# (the key from developer.apple.com › Keys, "Apple Push Notifications service").
set -eu
cd "$(dirname "$0")/.."
ROOT="$PWD"
TUNNEL="${TUNNEL:-copilot-canvas-mac}"
PORT="${PORT:-47700}"
STATE="${STATE:-$HOME/.copilot-canvas-mock}"
# What the phone calls this machine (it can be renamed in the app).
NAME="${NAME:-$(scutil --get LocalHostName 2>/dev/null | tr 'A-Z' 'a-z' | tr -cd 'a-z0-9-' | cut -c1-40)}"
NAME="${NAME:-mac}"
WORKDIR="${WORKDIR:-$HOME}"
mkdir -p "$STATE"
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

running() { [ -f "$STATE/$1.pid" ] && kill -0 "$(cat "$STATE/$1.pid")" 2>/dev/null; }

stop_one() {
  if running "$1"; then
    kill "$(cat "$STATE/$1.pid")" 2>/dev/null || true
    sleep 1
  fi
  rm -f "$STATE/$1.pid"
}

ensure_tunnel() {
  devtunnel user show | grep -q '^Logged in' || { echo "Sign in first: devtunnel user login -g   (GitHub; or without -g: Microsoft)"; exit 1; }
  if ! devtunnel show "$TUNNEL" >/dev/null 2>&1; then
    devtunnel create "$TUNNEL" -d "Copilot Canvas: this Mac as a stand-in Cloud PC (mock service)" >/dev/null
    echo "Created the private tunnel $TUNNEL (only your account can connect)."
  fi
  if ! devtunnel show "$TUNNEL" | grep -Eq "^ +$PORT "; then
    devtunnel port create "$TUNNEL" -p "$PORT" >/dev/null
    echo "Added port $PORT to $TUNNEL."
  fi
}

start_mock() {
  stop_one mock
  # Notifications: the APNs key (argument, environment, or the one saved by a previous start).
  APNS_KEY="${APNS_KEY:-$(cat "$STATE/apns-key" 2>/dev/null || true)}"
  APNS_TEAM="${APNS_TEAM:-$(cat "$STATE/apns-team" 2>/dev/null || true)}"
  set --
  if [ -n "$APNS_KEY" ] && [ -n "$APNS_TEAM" ]; then
    printf '%s' "$APNS_KEY" >"$STATE/apns-key"
    printf '%s' "$APNS_TEAM" >"$STATE/apns-team"
    set -- --apns-key "$APNS_KEY" --apns-team "$APNS_TEAM"
    [ -n "${APNS_KEY_ID:-}" ] && set -- "$@" --apns-key-id "$APNS_KEY_ID"
  fi
  # Real agent CLIs found on the PATH the mock starts with.
  PATH="$HOME/.local/bin:$HOME/.opencode/bin:/opt/homebrew/bin:$PATH" nohup "$ROOT/target/release/canvas-mock" "$@" \
    --listen "127.0.0.1:$PORT" --tunnel "$TUNNEL" --real-agents --name "$NAME" \
    --state "$STATE" --cwd "$WORKDIR" --qr "$STATE/pair-qr.png" >"$STATE/mock.log" 2>&1 </dev/null &
  echo $! >"$STATE/mock.pid"
  # Wait for the pairing link (the token comes from devtunnel).
  for _ in $(seq 1 60); do
    grep -q '^Pairing link:' "$STATE/mock.log" 2>/dev/null && break
    running mock || { cat "$STATE/mock.log"; exit 1; }
    sleep 0.5
  done
}

start_host() {
  if running host; then return; fi
  # caffeinate: the Mac stays awake while it hosts (on power).
  nohup caffeinate -is devtunnel host "$TUNNEL" >"$STATE/host.log" 2>&1 </dev/null &
  echo $! >"$STATE/host.pid"
  for _ in $(seq 1 60); do
    grep -q 'Ready to accept connections' "$STATE/host.log" 2>/dev/null && return
    running host || { cat "$STATE/host.log"; exit 1; }
    sleep 0.5
  done
  echo "The tunnel host did not report ready; see $STATE/host.log"
}

show_pairing() {
  echo
  grep -E '^(Pairing code|Pairing link):' "$STATE/mock.log" || true
  echo "Scan $STATE/pair-qr.png with the app (Pair → Scan), or paste the link (Pair → Or paste the pairing link)."
  echo "The code is single use per pairing and changes when the mock restarts; anyone with it and the token can pair: keep it to yourself."
  [ "${NO_OPEN:-}" = "1" ] || open "$STATE/pair-qr.png" 2>/dev/null || true
}

case "${1:-start}" in
  start)
    ensure_tunnel
    cargo build --release -q -p canvas-mock
    start_mock
    start_host
    show_pairing
    ;;
  pair)
    start_mock
    show_pairing
    ;;
  status)
    running mock && echo "mock: running (pid $(cat "$STATE/mock.pid"))" || echo "mock: stopped"
    running host && echo "tunnel host: running (pid $(cat "$STATE/host.pid"))" || echo "tunnel host: stopped"
    devtunnel show "$TUNNEL" 2>/dev/null | grep -E 'Tunnel ID|connections|Ports|^ +[0-9]' || true
    grep -E '^Pairing (code|link):' "$STATE/mock.log" 2>/dev/null || true
    ;;
  stop)
    stop_one mock
    stop_one host
    pkill -f "devtunnel host $TUNNEL" 2>/dev/null || true
    echo "Stopped."
    ;;
  logs)
    tail -f "$STATE/mock.log" "$STATE/host.log"
    ;;
  report)
    # The mock turns files in its publish folder into reports (the file name is the title);
    # phones that are away get a notification (with an APNs key).
    [ -f "${2:-}" ] || { echo "usage: ios/cloud-mac.sh report FILE.html|FILE.md|FILE.txt"; exit 2; }
    running mock || { echo "The mock is not running: ios/cloud-mac.sh start"; exit 1; }
    mkdir -p "$STATE/publish"
    cp "$2" "$STATE/publish/.$(basename "$2")"
    mv "$STATE/publish/.$(basename "$2")" "$STATE/publish/$(basename "$2")"
    echo "Published $(basename "$2") to the inbox of $NAME."
    ;;
  *)
    sed -n '2,16p' "$0"
    exit 2
    ;;
esac
