#!/usr/bin/env bash
# The Linux HTML view spike (docs/MAC-LINUX-PLAN.md §5.7): renders the same
# sanitized pages with WebKitGTK offscreen (X11 and Wayland, several lock-down
# variants), Blitz on the CPU and headless Chrome (the WebView2 engine, the
# reference), and writes PNGs and JSON results to OUT (CI job linux-html-spike).
set -u
here=$(cd "$(dirname "$0")" && pwd)
out=${1:-$here/out}
W=1000 H=800
mkdir -p "$out"
(cd "$here/blitz" && cargo build --release -q) || exit 1
blitz=$here/blitz/target/release/linux-html-blitz
for p in brief parity; do "$blitz" doc "$here/pages/$p.html" "$out/$p.doc.html"; done
cp "$here/pages/raw.html" "$out/raw.html"
gcc -O2 -Wall -o "$out/webkitgtk" "$here/webkitgtk.c" $(pkg-config --cflags --libs webkit2gtk-4.1) || exit 1
pkg-config --modversion webkit2gtk-4.1 > "$out/webkitgtk-version.txt"

run_variant() { # NAME DISPLAY-WRAPPER... -- FLAGS...
  local name=$1; shift
  local dir=$out/$name
  mkdir -p "$dir"
  cp "$out"/brief.doc.html "$out"/parity.doc.html "$out"/raw.html "$dir"/
  local wrap=()
  while [ "$1" != "--" ]; do wrap+=("$1"); shift; done
  shift
  echo "== $name"
  timeout 120 "${wrap[@]}" dbus-run-session -- "$out/webkitgtk" "$dir" $W $H "$@" > "$dir/result.json" 2> "$dir/stderr.log"
  echo "exit $?" >> "$dir/stderr.log"
  cat "$dir/result.json"
  tail -5 "$dir/stderr.log"
}

X=(xvfb-run -a -s "-screen 0 1280x1024x24" env GDK_BACKEND=x11)
run_variant x11-js "${X[@]}" -- --js
run_variant x11-nojs "${X[@]}" --
run_variant x11-js-sandbox "${X[@]}" -- --js --sandbox
run_variant x11-js-proxy-only "${X[@]}" -- --js --no-filter --proxy
run_variant x11-js-filter-and-proxy "${X[@]}" -- --js --proxy

# Wayland: a headless weston (9 on 22.04 takes the backend's file name).
export XDG_RUNTIME_DIR=$(mktemp -d)
chmod 700 "$XDG_RUNTIME_DIR"
(weston --backend=headless --socket=wl-spike --idle-time=0 2> "$out/weston.log" || weston --backend=headless-backend.so --socket=wl-spike --idle-time=0 2>> "$out/weston.log") &
sleep 3
run_variant wayland-js env WAYLAND_DISPLAY=wl-spike GDK_BACKEND=wayland -- --js
kill %1 2>/dev/null

echo "== blitz"
for p in brief parity; do "$blitz" render "$out/$p.doc.html" "$out/$p.blitz.png" $W $H | tee "$out/$p.blitz.json"; done

echo "== chrome (reference)"
chrome=$(command -v google-chrome || command -v chromium || true)
if [ -n "$chrome" ]; then
  for p in brief parity; do
    "$chrome" --headless=new --disable-gpu --hide-scrollbars --no-first-run --user-data-dir="$(mktemp -d)" \
      --screenshot="$out/$p.chrome.png" --window-size=$W,$H "file://$out/$p.doc.html" 2>/dev/null
  done
  "$chrome" --version > "$out/chrome-version.txt"
fi
ls -la "$out"
