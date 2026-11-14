'use strict';
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);
let lastPin = null;

async function refresh() {
  try {
    const info = await invoke('qr_info');
    $('qr').innerHTML = info.svg; // SVG generated locally by the app (no user content)
    $('url').textContent = info.url;
    $('pin').textContent = info.pin;
    lastPin = info.pin;
    $('err').textContent = '';
  } catch (e) {
    $('qr').replaceChildren();
    $('err').textContent = String(e);
  }
}

listen('state', (e) => {
  const s = e.payload;
  $('viewers').textContent = s.viewers === 1 ? '1 device watching' : `${s.viewers} devices watching`;
  if (s.pin !== lastPin) refresh(); // new PIN → old QR stops working, show the new one
});
refresh();
