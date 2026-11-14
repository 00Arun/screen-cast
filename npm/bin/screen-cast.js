#!/usr/bin/env node
// Screen Cast installer CLI. Zero dependencies.
//
// Downloads the Screen Cast desktop app for this computer from the project's GitHub Release
// that matches this package's version, verifies its SHA-256 checksum against the release's
// SHA256SUMS.txt, then installs and (optionally) opens it.
'use strict';

const fs = require('fs');
const os = require('os');
const path = require('path');
const crypto = require('crypto');
const { spawn, spawnSync } = require('child_process');

const REPO = '00Arun/screen-cast';
const PKG = require('../package.json');
const APP_NAME = 'Screen Cast';

const HELP = `Screen Cast ${PKG.version}: sandboxed browser that casts to your devices

Usage:
  npx screen-cast [command] [options]

Commands:
  install      Download, verify and install the app (default)
  open         Open the installed app
  uninstall    Remove the installed app
  where        Print where the app is installed
  help         Show this help

Options:
  --version <x.y.z>   Install a specific release (default: ${PKG.version})
  --no-open           Don't open the app after installing
  -v, --cli-version   Print the CLI version

More: https://github.com/${REPO}#readme`;

// ---------------------------------------------------------------------------------------------
// Platform
// ---------------------------------------------------------------------------------------------

function assetName(version, platform = process.platform, arch = process.arch) {
  if (platform === 'darwin') return `ScreenCast-${version}-macos-universal.dmg`;
  if (platform === 'win32') {
    if (arch !== 'x64' && arch !== 'arm64') throw new Error(`Unsupported Windows architecture: ${arch}`);
    return `ScreenCast-${version}-windows-x64-setup.exe`; // runs on arm64 via emulation
  }
  if (platform === 'linux') {
    if (arch !== 'x64') throw new Error(`Only x86_64 Linux builds are published (this is ${arch}).`);
    return `ScreenCast-${version}-linux-x86_64.AppImage`;
  }
  throw new Error(`Unsupported platform: ${platform}`);
}

function installPaths() {
  const home = os.homedir();
  if (process.platform === 'darwin') {
    let dir = '/Applications';
    try {
      fs.accessSync(dir, fs.constants.W_OK);
    } catch {
      dir = path.join(home, 'Applications');
    }
    return { app: path.join(dir, `${APP_NAME}.app`) };
  }
  if (process.platform === 'win32') {
    const local = process.env.LOCALAPPDATA || path.join(home, 'AppData', 'Local');
    return { dir: path.join(local, APP_NAME) };
  }
  const data = process.env.XDG_DATA_HOME || path.join(home, '.local', 'share');
  return {
    appImage: path.join(data, 'screen-cast', 'ScreenCast.AppImage'),
    icon: path.join(data, 'screen-cast', 'icon.png'),
    desktop: path.join(data, 'applications', 'screen-cast.desktop'),
  };
}

// ---------------------------------------------------------------------------------------------
// Download + verify
// ---------------------------------------------------------------------------------------------

async function fetchOk(url) {
  if (!url.startsWith('https://github.com/') && !url.startsWith('https://objects.githubusercontent.com/')) {
    throw new Error(`Refusing to download from ${url}`);
  }
  const res = await fetch(url, { redirect: 'follow', headers: { 'User-Agent': `screen-cast-cli/${PKG.version}` } });
  if (!res.ok) throw new Error(`Download failed (${res.status}) for ${url}`);
  return res;
}

function parseSums(text) {
  const map = new Map();
  for (const line of text.split(/\r?\n/)) {
    const m = line.trim().match(/^([0-9a-f]{64})\s+\*?(.+)$/i);
    if (m) map.set(m[2].trim(), m[1].toLowerCase());
  }
  return map;
}

async function download(version, name, dest) {
  const base = `https://github.com/${REPO}/releases/download/v${version}`;
  const sums = parseSums(await (await fetchOk(`${base}/SHA256SUMS.txt`)).text());
  const expected = sums.get(name);
  if (!expected) throw new Error(`No checksum for ${name} in release v${version}; refusing to install.`);

  const res = await fetchOk(`${base}/${encodeURIComponent(name)}`);
  const total = Number(res.headers.get('content-length')) || 0;
  const hash = crypto.createHash('sha256');
  const out = fs.createWriteStream(dest, { mode: 0o600 });
  let got = 0;
  let lastPct = -1;
  for await (const chunk of res.body) {
    hash.update(chunk);
    got += chunk.length;
    if (!out.write(chunk)) await new Promise((r) => out.once('drain', r));
    if (total && process.stderr.isTTY) {
      const pct = Math.floor((got / total) * 100);
      if (pct !== lastPct) {
        process.stderr.write(`\r  Downloading ${name}  ${pct}%`);
        lastPct = pct;
      }
    }
  }
  await new Promise((resolve, reject) => out.end((err) => (err ? reject(err) : resolve())));
  if (process.stderr.isTTY) process.stderr.write('\n');
  const actual = hash.digest('hex');
  if (actual !== expected) {
    fs.rmSync(dest, { force: true });
    throw new Error(`Checksum mismatch for ${name} (expected ${expected}, got ${actual}). Not installing.`);
  }
  console.log(`  ✓ Verified SHA-256 ${actual.slice(0, 16)}…`);
}

function run(cmd, args, opts = {}) {
  const r = spawnSync(cmd, args, { stdio: 'inherit', ...opts });
  if (r.error) throw r.error;
  if (r.status !== 0) throw new Error(`${cmd} ${args.join(' ')} exited with ${r.status}`);
}

// ---------------------------------------------------------------------------------------------
// Install / open / uninstall
// ---------------------------------------------------------------------------------------------

function installMac(file, app = installPaths().app) {
  const mount = fs.mkdtempSync(path.join(os.tmpdir(), 'screen-cast-dmg-'));
  run('hdiutil', ['attach', '-nobrowse', '-readonly', '-quiet', '-mountpoint', mount, file]);
  try {
    const src = path.join(mount, `${APP_NAME}.app`);
    if (!fs.existsSync(src)) throw new Error('The disk image does not contain the app.');
    fs.mkdirSync(path.dirname(app), { recursive: true });
    fs.rmSync(app, { recursive: true, force: true });
    run('ditto', [src, app]);
  } finally {
    spawnSync('hdiutil', ['detach', '-quiet', mount]);
    fs.rmSync(mount, { recursive: true, force: true });
  }
  return app;
}

function installWindows(file) {
  // NSIS installer, per-user (no admin prompt), silent.
  run(file, ['/S']);
  return installPaths().dir;
}

function installLinux(file) {
  const p = installPaths();
  fs.mkdirSync(path.dirname(p.appImage), { recursive: true });
  fs.copyFileSync(file, p.appImage);
  fs.chmodSync(p.appImage, 0o755);
  fs.copyFileSync(path.join(__dirname, '..', 'assets', 'icon.png'), p.icon);
  fs.mkdirSync(path.dirname(p.desktop), { recursive: true });
  fs.writeFileSync(
    p.desktop,
    [
      '[Desktop Entry]',
      'Type=Application',
      `Name=${APP_NAME}`,
      'Comment=Sandboxed browser that casts to your devices',
      `Exec="${p.appImage}"`,
      `Icon=${p.icon}`,
      'Categories=AudioVideo;Video;Network;',
      'Terminal=false',
      '',
    ].join('\n'),
  );
  return p.appImage;
}

function windowsExe() {
  const { dir } = installPaths();
  if (!fs.existsSync(dir)) return null;
  const exe = fs.readdirSync(dir).find((f) => f.toLowerCase().endsWith('.exe') && !/^uninstall/i.test(f));
  return exe ? path.join(dir, exe) : null;
}

function installedLocation() {
  const p = installPaths();
  if (process.platform === 'darwin') return fs.existsSync(p.app) ? p.app : null;
  if (process.platform === 'win32') return windowsExe();
  return fs.existsSync(p.appImage) ? p.appImage : null;
}

function openApp() {
  const where = installedLocation();
  if (!where) throw new Error('Screen Cast is not installed. Run: npx screen-cast install');
  if (process.platform === 'darwin') {
    run('open', [where]);
  } else {
    spawn(where, [], { detached: true, stdio: 'ignore' }).unref();
  }
  console.log(`  ✓ Opened ${where}`);
}

function uninstall() {
  const p = installPaths();
  if (process.platform === 'darwin') {
    fs.rmSync(p.app, { recursive: true, force: true });
  } else if (process.platform === 'win32') {
    const un = path.join(p.dir, 'uninstall.exe');
    if (fs.existsSync(un)) run(un, ['/S']);
  } else {
    for (const f of [p.appImage, p.icon, p.desktop]) fs.rmSync(f, { force: true });
  }
  console.log('  ✓ Screen Cast removed');
}

async function install(version, open) {
  const name = assetName(version);
  console.log(`Screen Cast ${version} for ${process.platform}/${process.arch}`);
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'screen-cast-'));
  const file = path.join(tmp, name);
  try {
    await download(version, name, file);
    if (process.platform === 'win32') fs.chmodSync(file, 0o700);
    let where;
    if (process.platform === 'darwin') where = installMac(file);
    else if (process.platform === 'win32') where = installWindows(file);
    else where = installLinux(file);
    console.log(`  ✓ Installed to ${where}`);
    if (process.platform === 'darwin') {
      console.log('  Note: on first Cast, allow "Screen & System Audio Recording" for Screen Cast, then reopen it.');
    }
    if (open) openApp();
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

// ---------------------------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------------------------

function parseArgs(argv) {
  const opts = { command: 'install', version: PKG.version, open: true };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--version') {
      opts.version = argv[++i] || '';
    } else if (a === '--no-open') {
      opts.open = false;
    } else if (a === '-v' || a === '--cli-version') {
      opts.command = 'cli-version';
    } else if (a === '-h' || a === '--help') {
      opts.command = 'help';
    } else if (!a.startsWith('-')) {
      opts.command = a;
    } else {
      throw new Error(`Unknown option: ${a}`);
    }
  }
  if (!/^\d+\.\d+\.\d+$/.test(opts.version)) throw new Error(`Invalid version: ${opts.version}`);
  return opts;
}

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  switch (opts.command) {
    case 'install':
      return install(opts.version, opts.open);
    case 'open':
      return openApp();
    case 'uninstall':
      return uninstall();
    case 'where':
      return console.log(installedLocation() || 'not installed');
    case 'cli-version':
      return console.log(PKG.version);
    case 'help':
      return console.log(HELP);
    default:
      throw new Error(`Unknown command: ${opts.command}\n\n${HELP}`);
  }
}

if (require.main === module) {
  main().catch((err) => {
    console.error(`✗ ${err.message}`);
    process.exit(1);
  });
}

module.exports = { assetName, parseSums, parseArgs, installMac };
