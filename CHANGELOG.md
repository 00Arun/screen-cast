# Changelog

All notable changes to this project are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses [Semantic Versioning](https://semver.org/).

## [1.0.2] - 2026-11-14

### Added
- `screen-cast` on npm: a web library to add secure casting to any web app. `CastHost` / `CastViewer` API, `<screen-cast-host>` / `<screen-cast-viewer>` web components and a zero-dependency Node signaling server (PIN + join link, lock-out, PIN rotation, IP-bound tokens, strict signaling).
- CI on macOS, Windows and Linux, and an automatic release pipeline that builds the installers for every OS and publishes them as a GitHub Release with checksums.

### Security
- Filtering proxy for all website traffic: blocks destinations that resolve to loopback/LAN/link-local/CGNAT addresses (DNS tricks and rebinding). WebRTC is disabled for websites.
- Fullscreen needs a user gesture, can't be spoofed or forced back after you exit, and is rate-limited. The PIN never flashes into the stream.
- Windows: frame navigation policy, external app launches cancelled, per-process audio capture.
- Linux: per-app audio capture, private runtime directory for filters.
- Server: fresh PIN per cast, join only while casting, network-wide wrong-PIN budget, random QR secret instead of the PIN, IP-bound sessions ended on stop, connection cap and header timeout, atomic viewer cap.
- Hardened injected script (prototype-level locks, copy-event protection), native back/forward, debug switches limited to debug builds.

## [1.0.1] - 2026-11-14

### Added
- About window with version and developer information.

## [1.0.0] - 2026-11-14

### Added
- First release: sandboxed browser with ad/tracker/malware blocking, own-window capture, H.264 + Opus over WebRTC to any browser on the LAN, PIN and QR code access, in-page fullscreen, app-only audio on macOS, Windows and Linux ports.
