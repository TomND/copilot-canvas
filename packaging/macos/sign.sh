#!/bin/sh
# Signs Copilot Canvas.app with the owner's Developer ID and notarizes it and
# its DMG (MAC-LINUX-PLAN §8.1, §11 item 17). Until the owner adds the
# secrets it says so and leaves the ad hoc signed build as it is (exit 0).
#
#   packaging/macos/sign.sh [DIR]   (DIR: build.sh's output, target/macos-package)
#
# Environment (GitHub secrets in CI):
#   MACOS_CERTIFICATE_P12      the Developer ID Application certificate, base64 (.p12)
#   MACOS_CERTIFICATE_PASSWORD its password
#   MACOS_SIGN_IDENTITY        e.g. "Developer ID Application: Name (TEAMID)"
#   APPSTORE_API_KEY_P8        App Store Connect API key, base64 (.p8)
#   APPSTORE_API_KEY_ID, APPSTORE_API_ISSUER_ID
set -eu

root="$(cd "$(dirname "$0")/../.." && pwd)"
out="${1:-$root/target/macos-package}"
app="$out/Copilot Canvas.app"
dmg="$(ls "$out"/CopilotCanvas-*-macos-*.dmg 2>/dev/null | head -1)"

if [ -z "${MACOS_CERTIFICATE_P12:-}" ] || [ -z "${MACOS_SIGN_IDENTITY:-}" ]; then
    echo "sign.sh: no Developer ID certificate (MACOS_CERTIFICATE_P12, MACOS_SIGN_IDENTITY): the build stays ad hoc signed and is not notarized"
    exit 0
fi
[ -d "$app" ] || { echo "sign.sh: $app is missing (run packaging/macos/build.sh)" >&2; exit 1; }

# A keychain of its own for the certificate, removed afterwards.
work="$(mktemp -d)"
keychain="$work/signing.keychain-db"
password="$(uuidgen)"
cleanup() {
    security delete-keychain "$keychain" >/dev/null 2>&1 || true
    rm -rf "$work"
}
trap cleanup EXIT
echo "$MACOS_CERTIFICATE_P12" | base64 --decode > "$work/cert.p12"
security create-keychain -p "$password" "$keychain"
security set-keychain-settings -lut 3600 "$keychain"
security unlock-keychain -p "$password" "$keychain"
security import "$work/cert.p12" -k "$keychain" -P "${MACOS_CERTIFICATE_PASSWORD:-}" -T /usr/bin/codesign
security set-key-partition-list -S apple-tool:,apple: -s -k "$password" "$keychain" >/dev/null
security list-keychains -d user -s "$keychain" $(security list-keychains -d user | tr -d '"')

# Inner programs first, then the bundle: hardened runtime, secure timestamp.
for bin in copilot-canvas copilot-canvas-gui; do
    codesign --force --sign "$MACOS_SIGN_IDENTITY" --keychain "$keychain" --options runtime --timestamp "$app/Contents/MacOS/$bin"
done
codesign --force --sign "$MACOS_SIGN_IDENTITY" --keychain "$keychain" --options runtime --timestamp "$app"
codesign --verify --strict --verbose=2 "$app"

if [ -z "${APPSTORE_API_KEY_P8:-}" ] || [ -z "${APPSTORE_API_KEY_ID:-}" ] || [ -z "${APPSTORE_API_ISSUER_ID:-}" ]; then
    echo "sign.sh: signed with $MACOS_SIGN_IDENTITY; no App Store Connect API key: not notarized"
    exit 0
fi
echo "$APPSTORE_API_KEY_P8" | base64 --decode > "$work/key.p8"
notary() {
    xcrun notarytool submit "$1" --key "$work/key.p8" --key-id "$APPSTORE_API_KEY_ID" --issuer "$APPSTORE_API_ISSUER_ID" --wait
}

# The app (zipped for the upload), then a DMG made from the signed app.
ditto -c -k --keepParent "$app" "$work/app.zip"
notary "$work/app.zip"
xcrun stapler staple "$app"
if [ -n "$dmg" ]; then
    stage="$work/dmg"
    mkdir -p "$stage"
    cp -R "$app" "$stage/"
    ln -s /Applications "$stage/Applications"
    hdiutil create -quiet -volname "Copilot Canvas" -srcfolder "$stage" -fs HFS+ -format UDZO -ov "$dmg"
    codesign --force --sign "$MACOS_SIGN_IDENTITY" --keychain "$keychain" --timestamp "$dmg"
    notary "$dmg"
    xcrun stapler staple "$dmg"
    sha="$(shasum -a 256 "$dmg" | cut -d' ' -f1)"
    echo "$sha  $(basename "$dmg")" > "$dmg.sha256"
    if [ -f "$out/Casks/copilot-canvas.rb" ]; then
        sed -i '' "s/^  sha256 .*/  sha256 \"$sha\"/" "$out/Casks/copilot-canvas.rb"
    fi
fi
spctl --assess --type execute --verbose=2 "$app"
echo "sign.sh: signed, notarized and stapled"
