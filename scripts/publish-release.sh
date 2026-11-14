#!/bin/bash
# Publishes the current version as a GitHub Release and uploads the installers, so anyone can
# download them from the repository's "Releases" page.
#
#   scripts/publish-release.sh                    # uploads dist/ScreenCast-<version>.dmg
#   scripts/publish-release.sh path/to/file ...   # uploads the given installers
#
# Windows/Linux installers are built on those systems; upload them to the same release with:
#   gh release upload v<version> <file> --clobber
# Requires the GitHub CLI (https://cli.github.com) logged in (`gh auth login`), run inside the repo.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(tr -d '[:space:]' < "$ROOT/VERSION")"
TAG="v$VERSION"
FILES=("$@")
if [[ ${#FILES[@]} -eq 0 ]]; then
  FILES=("$ROOT/dist/ScreenCast-$VERSION.dmg")
fi
for f in "${FILES[@]}"; do
  [[ -f "$f" ]] || { echo "missing: $f (build it first)" >&2; exit 1; }
done

if gh release view "$TAG" >/dev/null 2>&1; then
  gh release upload "$TAG" "${FILES[@]}" --clobber
else
  gh release create "$TAG" "${FILES[@]}" --title "Screen Cast $VERSION" \
    --notes "Screen Cast $VERSION. Download the installer for your system below. See the README for install steps."
fi
echo "✓ Published $TAG"
