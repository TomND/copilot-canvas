#!/bin/sh
# Builds Atelier for macOS (MAC-LINUX-PLAN §8.1): the universal
# Atelier.app (arm64 and x86_64 slices joined with lipo; the GUI is the
# bundle executable, atelier beside it is the service, CLI and TUI),
# a DMG, its SHA-256 and the Homebrew cask filled in with it.
#
# The bundle is signed ad hoc with the hardened runtime; packaging/macos/sign.sh
# signs it with a Developer ID and notarizes it when the owner's secrets are there.
#
#   packaging/macos/build.sh [--arch universal|arm64|x86_64] [--out DIR] [--skip-build]
#
# Needs: Xcode's command line tools (lipo, codesign, iconutil, sips, hdiutil),
# the Rust targets, and Ghostty's archives for them (cargo xtask build-ghostty
# --target aarch64-apple-darwin / x86_64-apple-darwin).
set -eu

root="$(cd "$(dirname "$0")/../.." && pwd)"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
arch=universal
out="$root/target/macos-package"
build=1
while [ $# -gt 0 ]; do
    case "$1" in
        --arch) arch="$2"; shift 2 ;;
        --out) out="$2"; shift 2 ;;
        --skip-build) build=0; shift ;;
        *) echo "unknown option $1" >&2; exit 2 ;;
    esac
done
case "$arch" in
    universal) targets="aarch64-apple-darwin x86_64-apple-darwin" ;;
    arm64) targets="aarch64-apple-darwin" ;;
    x86_64) targets="x86_64-apple-darwin" ;;
    *) echo "--arch: universal, arm64 or x86_64" >&2; exit 2 ;;
esac

# Release binaries for each slice (Ghostty linked statically per target).
if [ "$build" = 1 ]; then
    for t in $targets; do
        echo "== cargo build --release --target $t"
        (cd "$root" && MACOSX_DEPLOYMENT_TARGET=13.0 cargo build --release --target "$t" --bin atelier --bin atelier-gui)
    done
fi

rm -rf "$out"
mkdir -p "$out"
app="$out/Atelier.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

# One file per program with every slice (paths may hold spaces).
for bin in atelier atelier-gui; do
    set --
    for t in $targets; do
        set -- "$@" "$root/target/$t/release/$bin"
    done
    lipo -create "$@" -output "$app/Contents/MacOS/$bin"
    chmod 755 "$app/Contents/MacOS/$bin"
done
lipo -info "$app/Contents/MacOS/atelier"

sed "s/@VERSION@/$version/g" "$root/packaging/macos/Info.plist" > "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist" >/dev/null
printf 'APPL????' > "$app/Contents/PkgInfo"

# The icon: every size an .icns holds, from the 512 px drawing.
iconset="$out/AppIcon.iconset"
mkdir -p "$iconset"
src="$root/assets/icon/atelier.png"
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$src" --out "$iconset/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z "$double" "$double" "$src" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"
rm -rf "$iconset"

# Ad hoc signatures with the hardened runtime, inner programs first (sign.sh
# replaces them with Developer ID ones). No entitlements: no sandbox, no JIT.
for bin in atelier atelier-gui; do
    codesign --force --sign - --options runtime --timestamp=none "$app/Contents/MacOS/$bin"
done
codesign --force --sign - --options runtime --timestamp=none "$app"
codesign --verify --strict --verbose=1 "$app"

# The DMG: the app and a link to /Applications.
suffix="$arch"
[ "$arch" = arm64 ] && suffix=arm64
dmg="$out/Atelier-$version-macos-$suffix.dmg"
stage="$out/dmg"
mkdir -p "$stage"
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"
hdiutil create -quiet -volname "Atelier" -srcfolder "$stage" -fs HFS+ -format UDZO -ov "$dmg"
rm -rf "$stage"
sha="$(shasum -a 256 "$dmg" | cut -d' ' -f1)"
echo "$sha  $(basename "$dmg")" > "$dmg.sha256"

# The cask for this DMG (the tap's copy is packaging/homebrew-tap/Casks).
mkdir -p "$out/Casks"
sed -e "s/^  version \".*\"/  version \"$version\"/" -e "s/^  sha256 .*/  sha256 \"$sha\"/" \
    "$root/packaging/homebrew-tap/Casks/atelier.rb" > "$out/Casks/atelier.rb"

echo "built $app ($version, $arch)"
echo "built $dmg ($sha)"
echo "built $out/Casks/atelier.rb"
