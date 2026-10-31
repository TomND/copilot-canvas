#!/bin/sh
# Installs Copilot Canvas from this folder into ~/.local (or PREFIX):
# bin/, the .desktop file, icons and metadata. `./install.sh --uninstall`
# removes them again; the data in ~/.copilot-canvas is left alone.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
prefix="${PREFIX:-$HOME/.local}"
files="bin/copilot-canvas bin/copilot-canvas-gui share/applications/copilot-canvas.desktop share/metainfo/io.github.tomnd.CopilotCanvas.metainfo.xml"
for size in 512 256 128 64 48 32; do
  files="$files share/icons/hicolor/${size}x${size}/apps/copilot-canvas.png"
done
if [ "${1:-}" = "--uninstall" ]; then
  for f in $files; do rm -f "$prefix/$f"; done
  echo "Removed Copilot Canvas from $prefix."
  exit 0
fi
for f in $files; do
  mkdir -p "$(dirname "$prefix/$f")"
  cp "$here/$f" "$prefix/$f"
done
chmod 755 "$prefix/bin/copilot-canvas" "$prefix/bin/copilot-canvas-gui"
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$prefix/share/applications" 2>/dev/null || true
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t "$prefix/share/icons/hicolor" 2>/dev/null || true
echo "Installed Copilot Canvas into $prefix (make sure $prefix/bin is on PATH)."
