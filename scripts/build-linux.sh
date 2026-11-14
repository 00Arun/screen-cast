#!/bin/bash
# Builds Screen Cast for Linux (.deb + .AppImage). Tested target: Ubuntu 22.04+/Debian 12+.
# Usage: scripts/build-linux.sh
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

if command -v apt-get >/dev/null; then
  sudo apt-get update
  sudo apt-get install -y build-essential curl wget file cmake pkg-config libssl-dev \
    libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev libayatana-appindicator3-dev \
    libpulse-dev libxdo-dev bubblewrap
elif command -v dnf >/dev/null; then
  sudo dnf install -y gcc gcc-c++ make cmake pkgconf-pkg-config openssl-devel curl wget file \
    webkit2gtk4.1-devel gtk3-devel librsvg2-devel libappindicator-gtk3-devel \
    pulseaudio-libs-devel libxdo-devel bubblewrap
else
  echo "Install the WebKitGTK 4.1, GTK 3, PulseAudio and CMake development packages for your distro." >&2
fi

command -v cargo >/dev/null || { curl https://sh.rustup.rs -sSf | sh -s -- -y; source "$HOME/.cargo/env"; }
cargo install tauri-cli --version "^2" --locked

# Every build gets a new version (patch +1). BUMP=minor|major, NO_BUMP=1 to keep it.
if [[ "${NO_BUMP:-0}" == "1" ]]; then VERSION="$(cat "$ROOT/VERSION")"; else VERSION="$("$ROOT/scripts/bump-version.sh" "${BUMP:-patch}")"; fi
echo "Building Screen Cast $VERSION"
cd "$ROOT/src-tauri"
cargo tauri build
echo
echo "✓ Installers:"
ls -1 target/release/bundle/deb/*.deb target/release/bundle/appimage/*.AppImage 2>/dev/null
