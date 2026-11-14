# screen-cast

Installer for **[Screen Cast](https://github.com/00Arun/screen-cast)**: a sandboxed browser that
casts what it shows to your phone, TV or laptop over Wi-Fi. Viewers receive only video and
sound, never the website, so ads and scripts on movie sites can't reach them.

![Screen Cast casting a movie](https://raw.githubusercontent.com/00Arun/screen-cast/main/docs/screenshots/app-casting.png)

## Install

```bash
npx screen-cast
```

This downloads the Screen Cast app for your computer from the matching
[GitHub Release](https://github.com/00Arun/screen-cast/releases), **verifies its SHA-256
checksum**, installs it and opens it.

| System | Installs to |
|---|---|
| macOS 14+ (Apple Silicon & Intel) | `/Applications/Screen Cast.app` (or `~/Applications`) |
| Windows 10 (2004+) / 11 | `%LOCALAPPDATA%\Screen Cast` (per user, no admin needed) |
| Linux x86_64 | `~/.local/share/screen-cast/ScreenCast.AppImage` + an app-menu entry |

## Commands

```bash
npx screen-cast              # install (or update) and open
npx screen-cast open         # open the installed app
npx screen-cast uninstall    # remove it
npx screen-cast where        # show where it is installed
npx screen-cast --version 1.0.2 --no-open   # a specific release, don't open
```

## What Screen Cast does

- 🛡 **Sandboxed browsing**: ad, tracker and malware blocking; no downloads, pop-ups or
  local-network access; macOS App Sandbox.
- 📡 **Cast to any browser** on the same Wi-Fi. Viewers scan a QR code or enter a PIN.
- 🎬 **True fullscreen**, 🔊 **app-only sound**, ⚡ **smooth WebRTC playback**.

See the [full README](https://github.com/00Arun/screen-cast#readme) for screenshots, the
security model and how to build from source.

## Security of this installer

- It has no dependencies.
- It downloads only from `github.com` over HTTPS, from the release whose version matches this
  package.
- It refuses to install if the file's SHA-256 doesn't match the release's `SHA256SUMS.txt`.
- Packages are published from GitHub Actions with
  [npm provenance](https://docs.npmjs.com/generating-provenance-statements).

## About

Developed by **Arun Pandey**, **Arun AI Studio**: [arun-pandey.com.np](https://arun-pandey.com.np/).
MIT licensed (this installer).
