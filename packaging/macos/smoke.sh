#!/bin/sh
# Installs Copilot Canvas from its DMG and runs it (MAC-LINUX-PLAN §8.1; CI's
# install test): the app copied out of the DMG into a folder, its signature,
# both slices (the Intel one under Rosetta), the doctor, the TUI self-test,
# and with --gui the GUI's self-test, all from the installed bundle. Every
# run uses its own throw-away home.
#
#   packaging/macos/smoke.sh DMG [--gui] [--into DIR]
set -eu

dmg="$1"
shift
gui=0
scratch="$(mktemp -d)"
into="$scratch/Applications"
while [ $# -gt 0 ]; do
    case "$1" in
        --gui) gui=1; shift ;;
        --into) into="$2"; shift 2 ;;
        *) echo "unknown option $1" >&2; exit 2 ;;
    esac
done
mkdir -p "$into"

mount="$(mktemp -d)"
hdiutil attach -quiet -nobrowse -readonly -noautoopen -mountpoint "$mount" "$dmg"
cp -R "$mount/Copilot Canvas.app" "$into/"
[ -L "$mount/Applications" ] || { echo "the DMG has no Applications link" >&2; exit 1; }
hdiutil detach -quiet "$mount"
app="$into/Copilot Canvas.app"
cli="$app/Contents/MacOS/copilot-canvas"
echo "== installed $app"

codesign --verify --strict --verbose=1 "$app"
plutil -lint "$app/Contents/Info.plist" >/dev/null
[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$app/Contents/Info.plist")" = copilot-canvas-gui ]
[ -f "$app/Contents/Resources/AppIcon.icns" ]
echo "slices: $(lipo -archs "$cli")"
"$cli" --version
if lipo -archs "$cli" | grep -q x86_64 && arch -x86_64 /usr/bin/true 2>/dev/null; then
    echo "== the Intel slice under Rosetta"
    arch -x86_64 "$cli" --version
    arch -x86_64 "$app/Contents/MacOS/copilot-canvas-gui" --version
fi

home="$(mktemp -d)"
echo "== doctor"
"$cli" --home "$home" --instance smoke$$ doctor || true
echo "== TUI self-test from the bundle"
"$cli" selftest
if [ "$gui" = 1 ]; then
    echo "== GUI self-test from the bundle"
    "$cli" selftest --client gui
fi
rm -rf "$home" "$scratch"
echo "PASS: installed and ran $(basename "$dmg")"
