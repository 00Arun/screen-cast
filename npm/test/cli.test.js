'use strict';
const assert = require('assert');
const { assetName, parseSums, parseArgs } = require('../bin/screen-cast.js');

assert.strictEqual(assetName('1.2.3', 'darwin', 'arm64'), 'ScreenCast-1.2.3-macos-universal.dmg');
assert.strictEqual(assetName('1.2.3', 'win32', 'x64'), 'ScreenCast-1.2.3-windows-x64-setup.exe');
assert.strictEqual(assetName('1.2.3', 'linux', 'x64'), 'ScreenCast-1.2.3-linux-x86_64.AppImage');
assert.throws(() => assetName('1.2.3', 'linux', 'arm64'));
assert.throws(() => assetName('1.2.3', 'freebsd', 'x64'));

const sums = parseSums(`${'a'.repeat(64)}  ScreenCast-1.2.3-macos-universal.dmg\n${'B'.repeat(64)} *ScreenCast-1.2.3-windows-x64-setup.exe\n`);
assert.strictEqual(sums.get('ScreenCast-1.2.3-macos-universal.dmg'), 'a'.repeat(64));
assert.strictEqual(sums.get('ScreenCast-1.2.3-windows-x64-setup.exe'), 'b'.repeat(64));

assert.deepStrictEqual(parseArgs([]).command, 'install');
assert.strictEqual(parseArgs(['--version', '2.0.0']).version, '2.0.0');
assert.strictEqual(parseArgs(['open']).command, 'open');
assert.strictEqual(parseArgs(['--no-open']).open, false);
assert.throws(() => parseArgs(['--version', '../../evil']));
assert.throws(() => parseArgs(['--bogus']));
console.log('cli tests passed');
