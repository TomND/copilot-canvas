#!/bin/sh
# Builds the Rust core (crates/canvas-mobile) for the iPhone and the
# simulator, writes its Swift bindings and packs CanvasMobileCore.xcframework.
# Run from anywhere; Xcode runs it before compiling (see project.yml).
#
#   ios/build-core.sh            release build (what the app ships)
#   PROFILE=dev ios/build-core.sh   faster debug build
set -eu
# Xcode's build phases export hundreds of settings (SDKROOT, platform and
# deployment variables, compiler flags) that point C builds inside cargo
# (zstd) at the wrong SDK: start over from a clean environment.
if [ -n "${XCODE_PRODUCT_BUILD_VERSION:-}" ] && [ -z "${CANVAS_CORE_CLEAN_ENV:-}" ]; then
  exec env -i CANVAS_CORE_CLEAN_ENV=1 HOME="$HOME" USER="${USER:-}" TMPDIR="${TMPDIR:-/tmp}" \
    PATH="$HOME/.cargo/bin:/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin" \
    PROFILE="${PROFILE:-release}" /bin/sh "$0" "$@"
fi
cd "$(dirname "$0")/.."
# Xcode's build phases run without the user's shell profile.
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
PROFILE="${PROFILE:-release}"
DIR="$PROFILE"
[ "$PROFILE" = "dev" ] && DIR=debug
OUT=ios/Core
GEN="$OUT/Generated"
unset SDKROOT 2>/dev/null || true
export IPHONEOS_DEPLOYMENT_TARGET=17.0

for target in aarch64-apple-ios aarch64-apple-ios-sim; do
  rustup target list --installed | grep -q "^$target\$" || rustup target add "$target"
  cargo build -p canvas-mobile --lib --profile "$PROFILE" --target "$target"
done
# The bindings are read from a host build of the library.
cargo build -p canvas-mobile --lib --profile "$PROFILE"
rm -rf "$GEN"
mkdir -p "$GEN"
cargo run -q -p canvas-mobile --features bindgen --profile "$PROFILE" --bin uniffi-bindgen -- \
  generate --library "target/$DIR/libcanvas_mobile.dylib" --language swift --out-dir "$GEN"

HEADERS="target/ios-headers"
rm -rf "$HEADERS"
mkdir -p "$HEADERS"
cp "$GEN"/*.h "$HEADERS/"
cat "$GEN"/*.modulemap > "$HEADERS/module.modulemap"
rm -rf "$OUT/CanvasMobileCore.xcframework"
xcodebuild -create-xcframework \
  -library "target/aarch64-apple-ios/$DIR/libcanvas_mobile.a" -headers "$HEADERS" \
  -library "target/aarch64-apple-ios-sim/$DIR/libcanvas_mobile.a" -headers "$HEADERS" \
  -output "$OUT/CanvasMobileCore.xcframework" >/dev/null
# Only the Swift side of the bindings is compiled by the app.
mkdir -p "$OUT/Swift"
cp "$GEN"/*.swift "$OUT/Swift/"
echo "CanvasMobileCore.xcframework and Swift bindings in $OUT"
