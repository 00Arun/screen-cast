'use strict';
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);
let editing = false;
let toastTimer = null;

function toast(text) {
  const t = $('toast');
  t.textContent = text;
  t.classList.remove('hidden');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.classList.add('hidden'), text.length > 80 ? 12000 : 4000);
}

function render(s) {
  document.body.classList.toggle('os-macos', s.platform === 'macos');
  if (!editing) $('url').value = s.url || '';
  $('blocked').textContent = `${s.blocked} blocked`;
  const cast = $('cast');
  cast.textContent = s.casting ? '● Stop' : 'Cast';
  cast.classList.toggle('live', s.casting);
  const info = $('castInfo');
  info.classList.toggle('hidden', !s.casting);
  $('pinBtn').classList.toggle('hidden', !s.casting);
  $('qrBtn').classList.toggle('hidden', !s.casting);
  if (s.casting) {
    info.replaceChildren();
    const add = (label, value) => {
      info.append(label);
      const b = document.createElement('b');
      b.textContent = value;
      info.append(b, '  ');
    };
    add('Open ', s.lanUrl || '(no Wi-Fi)');
    add('PIN ', s.pin);
    add('👁 ', String(s.viewers));
  }
}

const call = (cmd, args) => invoke(cmd, args).catch((e) => toast(String(e)));

$('nav').addEventListener('submit', (e) => {
  e.preventDefault();
  editing = false;
  $('url').blur();
  call('navigate', { input: $('url').value });
});
$('url').addEventListener('focus', () => { editing = true; $('url').select(); });
$('url').addEventListener('blur', () => { editing = false; });
$('back').addEventListener('click', () => call('go_back'));
$('fwd').addEventListener('click', () => call('go_forward'));
$('reload').addEventListener('click', () => call('reload'));
$('cast').addEventListener('click', async () => {
  $('cast').disabled = true;
  try { await invoke('toggle_cast'); } catch (e) { toast(String(e)); }
  $('cast').disabled = false;
});
$('pinBtn').addEventListener('click', () => call('new_pin'));
$('qrBtn').addEventListener('click', () => call('show_qr'));
$('about').addEventListener('click', () => call('show_about'));

listen('state', (e) => render(e.payload));
listen('blocked', (e) => toast(`Blocked: ${e.payload}`));
invoke('get_state').then(render);
