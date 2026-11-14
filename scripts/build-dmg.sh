#!/bin/bash
# Builds "Screen Cast.app" (App Sandbox, ad-hoc signed) and "ScreenCast.dmg".
# Usage: scripts/build-dmg.sh            (Apple Silicon)
#        UNIVERSAL=1 scripts/build-dmg.sh (Apple Silicon + Intel)
# Every build gets a new version (patch +1). BUMP=minor|major to bump more, NO_BUMP=1 to keep it.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [[ "${NO_BUMP:-0}" == "1" ]]; then VERSION="$(cat "$ROOT/VERSION")"; else VERSION="$("$ROOT/scripts/bump-version.sh" "${BUMP:-patch}")"; fi
echo "Building Screen Cast $VERSION"
TAURI="$ROOT/src-tauri"
OUT="$ROOT/dist"
APP="$OUT/Screen Cast.app"
export MACOSX_DEPLOYMENT_TARGET=14.0

cd "$TAURI"
if [[ "${UNIVERSAL:-0}" == "1" ]]; then
  rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
  cargo build --release --locked --target aarch64-apple-darwin
  cargo build --release --locked --target x86_64-apple-darwin
  BIN="$OUT/screen_cast"
  mkdir -p "$OUT"
  lipo -create -output "$BIN" \
    target/aarch64-apple-darwin/release/screen_cast target/x86_64-apple-darwin/release/screen_cast
else
  cargo build --release --locked
  BIN="$TAURI/target/release/screen_cast"
fi

rm -rf "$APP" && mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/screen_cast"
cp "$TAURI/macos/Info.plist" "$APP/Contents/Info.plist"
cp "$TAURI/icons/icon.icns" "$APP/Contents/Resources/icon.icns"
printf 'APPL????' > "$APP/Contents/PkgInfo"

# Ad-hoc signature with the App Sandbox entitlements (hardened runtime on).
# Signing:
#  - SIGN_IDENTITY="Developer ID Application: ..." (or any code-signing certificate in your
#    keychain): the app's identity is tied to that certificate, so the Screen Recording
#    permission survives updates AND no other app can claim it. Recommended for distribution.
#  - Default (no certificate): ad-hoc signature whose designated requirement is pinned to the
#    bundle id, so the permission survives updates. Trade-off: another local program signed
#    with the same identifier could match that permission.
if [[ -n "${SIGN_IDENTITY:-}" ]]; then
  codesign --force --deep --options runtime --timestamp --sign "$SIGN_IDENTITY" \
    --entitlements "$TAURI/macos/entitlements.plist" "$APP"
else
  codesign --force --deep --options runtime --sign - \
    --requirements '=designated => identifier "app.screencast.sandbox"' \
    --entitlements "$TAURI/macos/entitlements.plist" "$APP"
fi
codesign --verify --strict --verbose=2 "$APP"
codesign -d --entitlements - "$APP" 2>/dev/null | grep -q app-sandbox && echo "✓ App Sandbox enabled"

# DMG with an Applications shortcut for drag-to-install.
STAGE="$OUT/dmg-stage"
DMG="$OUT/ScreenCast-$VERSION.dmg"
rm -rf "$STAGE" "$DMG" && mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "Screen Cast $VERSION" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
rm -rf "$STAGE"
echo "✓ Built: $DMG"
