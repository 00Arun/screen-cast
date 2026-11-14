#!/bin/bash
# Increments the version for every build and writes it everywhere it is used.
#   scripts/bump-version.sh          1.2.3 -> 1.2.4   (default: patch)
#   scripts/bump-version.sh minor    1.2.3 -> 1.3.0
#   scripts/bump-version.sh major    1.2.3 -> 2.0.0
#   scripts/bump-version.sh set 2.0.0
# Prints the new version.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CUR="$(tr -d '[:space:]' < "$ROOT/VERSION")"
IFS=. read -r MA MI PA <<< "$CUR"
case "${1:-patch}" in
  major) NEW="$((MA + 1)).0.0" ;;
  minor) NEW="$MA.$((MI + 1)).0" ;;
  patch) NEW="$MA.$MI.$((PA + 1))" ;;
  set)   NEW="$2" ;;
  *) echo "usage: $0 [patch|minor|major|set X.Y.Z]" >&2; exit 1 ;;
esac
[[ "$NEW" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "bad version: $NEW" >&2; exit 1; }

echo "$NEW" > "$ROOT/VERSION"
T="$ROOT/src-tauri"
# Cargo.toml: only the [package] version (first match).
perl -0pi -e 's/(\[package\][^\[]*?\nversion = ")[^"]+(")/${1}'"$NEW"'${2}/' "$T/Cargo.toml"
perl -pi -e 's/("version":\s*")[^"]+(")/${1}'"$NEW"'${2}/' "$T/tauri.conf.json"
# npm installer package
[[ -f "$ROOT/npm/package.json" ]] && perl -pi -e 's/^(  "version":\s*")[^"]+(")/${1}'"$NEW"'${2}/' "$ROOT/npm/package.json"
# Cargo.lock entry of this package (so `cargo build --locked` keeps working).
[[ -f "$T/Cargo.lock" ]] && perl -0pi -e 's/(\[\[package\]\]\nname = "screen_cast"\nversion = ")[^"]+(")/${1}'"$NEW"'${2}/' "$T/Cargo.lock"
perl -0pi -e 's#(<key>CFBundleShortVersionString</key><string>)[^<]+#${1}'"$NEW"'#; s#(<key>CFBundleVersion</key><string>)[^<]+#${1}'"$NEW"'#' "$T/macos/Info.plist"
echo "$NEW"
