#!/usr/bin/env bash
# Builds the Linux packages of Copilot Canvas (MAC-LINUX-PLAN §8.2) from
# release binaries: a tarball (with install.sh), an AppImage, a .deb and an
# .rpm, for one architecture.
#
#   packaging/linux/build.sh --arch x86_64|aarch64 --bin target/release --out dist
#
# Needs: ImageMagick (icons), dpkg-deb, rpmbuild (the `rpm` package on Ubuntu)
# and network access for the pinned appimagetool and AppImage runtime.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
arch="" bin="" out=""
while [ $# -gt 0 ]; do
  case "$1" in
    --arch) arch="$2"; shift 2 ;;
    --bin) bin="$2"; shift 2 ;;
    --out) out="$2"; shift 2 ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
  esac
done
[ -n "$arch" ] && [ -n "$bin" ] && [ -n "$out" ] || { echo "usage: build.sh --arch ARCH --bin DIR --out DIR" >&2; exit 2; }
case "$arch" in
  x86_64) deb_arch=amd64; tool_sha=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0; runtime_sha=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d ;;
  aarch64) deb_arch=arm64; tool_sha=f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158; runtime_sha=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444 ;;
  *) echo "arch must be x86_64 or aarch64" >&2; exit 2 ;;
esac
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
bin="$(cd "$bin" && pwd)"
mkdir -p "$out"
out="$(cd "$out" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# The installed tree, shared by every format: usr/bin and usr/share.
stage="$work/stage"
install -Dm755 "$bin/copilot-canvas" "$stage/usr/bin/copilot-canvas"
install -Dm755 "$bin/copilot-canvas-gui" "$stage/usr/bin/copilot-canvas-gui"
strip "$stage/usr/bin/copilot-canvas" "$stage/usr/bin/copilot-canvas-gui" 2>/dev/null || true
install -Dm644 "$here/copilot-canvas.desktop" "$stage/usr/share/applications/copilot-canvas.desktop"
install -Dm644 "$here/io.github.tomnd.CopilotCanvas.metainfo.xml" "$stage/usr/share/metainfo/io.github.tomnd.CopilotCanvas.metainfo.xml"
convert_cmd="$(command -v magick || command -v convert)"
for size in 512 256 128 64 48 32; do
  mkdir -p "$stage/usr/share/icons/hicolor/${size}x${size}/apps"
  "$convert_cmd" "$root/assets/icon/copilot-canvas.png" -resize "${size}x${size}" "$stage/usr/share/icons/hicolor/${size}x${size}/apps/copilot-canvas.png"
done
install -Dm644 "$root/README.md" "$stage/usr/share/doc/copilot-canvas/README.md"

# Tarball: bin/ and share/ at the top, with install.sh (into ~/.local).
name="copilot-canvas-$version-linux-$arch"
mkdir -p "$work/$name"
cp -r "$stage/usr/bin" "$stage/usr/share" "$work/$name/"
install -m755 "$here/install.sh" "$work/$name/install.sh"
tar -C "$work" -czf "$out/$name.tar.gz" "$name"

# .deb (Ubuntu 22.04+, Debian 12+). The GUI loads Wayland, X11, Vulkan/GL
# and WebKitGTK at run time, so they are recommended, not required.
deb="$work/deb"
cp -r "$stage" "$deb"
mkdir -p "$deb/DEBIAN"
size_kb="$(du -sk "$deb/usr" | cut -f1)"
cat > "$deb/DEBIAN/control" <<CONTROL
Package: copilot-canvas
Version: $version
Architecture: $deb_arch
Maintainer: Copilot Canvas <noreply@github.com>
Installed-Size: $size_kb
Depends: libc6 (>= 2.35), libgcc-s1
Recommends: libwebkit2gtk-4.1-0, libxkbcommon0, libvulkan1, mesa-vulkan-drivers, libegl1, xdg-utils, fonts-noto-color-emoji, fonts-noto-core, gnome-keyring | kwalletmanager | keepassxc, bubblewrap, wl-clipboard | xclip
Suggests: git, gh
Section: devel
Priority: optional
Homepage: https://github.com/TomND/copilot-canvas
Description: Infinite canvas of AI coding-agent sessions in real terminals
 Copilot Canvas lays out GitHub Copilot CLI, OpenCode and Claude Code sessions
 on an infinite canvas, each in a real terminal hosted by a background service.
 It includes a GPU client, a terminal client, passive sessions, automations,
 a reports inbox and remote mode over Dev Tunnels.
CONTROL
dpkg-deb --root-owner-group --build "$deb" "$out/copilot-canvas_${version}_${deb_arch}.deb"

# .rpm (Fedora 39+, RHEL 9+).
rpmtop="$work/rpm"
mkdir -p "$rpmtop"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}
files="$(cd "$stage" && find . -type f | sed 's|^\.||' | sort)"
cat > "$rpmtop/SPECS/copilot-canvas.spec" <<SPEC
Name: copilot-canvas
Version: $version
Release: 1
Summary: Infinite canvas of AI coding-agent sessions in real terminals
License: LicenseRef-proprietary
URL: https://github.com/TomND/copilot-canvas
Requires: glibc >= 2.35
Recommends: webkit2gtk4.1, libxkbcommon, vulkan-loader, mesa-vulkan-drivers, mesa-libEGL, xdg-utils, google-noto-color-emoji-fonts, bubblewrap
AutoReqProv: no
%global debug_package %{nil}
%global __strip /bin/true

%description
Copilot Canvas lays out GitHub Copilot CLI, OpenCode and Claude Code sessions
on an infinite canvas, each in a real terminal hosted by a background service.

%install
mkdir -p %{buildroot}
cp -a $stage/. %{buildroot}/

%files
$files
SPEC
rpmbuild --define "_topdir $rpmtop" --target "$arch" -bb "$rpmtop/SPECS/copilot-canvas.spec" >/dev/null
cp "$rpmtop"/RPMS/*/*.rpm "$out/"

# AppImage: the GUI (AppRun), `AppRun cli …` the command line. The service
# runs from its per-build copy in ~/.copilot-canvas/bin, never from the mount.
appdir="$work/AppDir"
cp -r "$stage" "$appdir"
install -m755 "$here/AppRun" "$appdir/AppRun"
cp "$here/copilot-canvas.desktop" "$appdir/copilot-canvas.desktop"
cp "$stage/usr/share/icons/hicolor/256x256/apps/copilot-canvas.png" "$appdir/copilot-canvas.png"
ln -s copilot-canvas.png "$appdir/.DirIcon"
tool="$work/appimagetool"
curl -sSfL -o "$tool" "https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-$arch.AppImage"
echo "$tool_sha  $tool" | sha256sum -c - >/dev/null
curl -sSfL -o "$work/runtime" "https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-$arch"
echo "$runtime_sha  $work/runtime" | sha256sum -c - >/dev/null
chmod +x "$tool"
ARCH="$arch" "$tool" --appimage-extract-and-run --no-appstream --runtime-file "$work/runtime" "$appdir" "$out/Copilot_Canvas-$version-$arch.AppImage" >/dev/null
chmod +x "$out/Copilot_Canvas-$version-$arch.AppImage"

ls -la "$out"
