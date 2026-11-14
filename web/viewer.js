'use strict';
// Viewer: receives ONLY an encrypted WebRTC video/audio stream from the Mac.
// No web content from the site being watched ever reaches this page.

const $ = (id) => document.getElementById(id);
const video = $('video');
let pc = null;
let retryTimer = null;
let hideTimer = null;

function show(section) {
  $('join').hidden = section !== 'join';
  $('player').hidden = section !== 'player';
}

function status(text) {
  $('status').textContent = text;
}

async function post(path, body, type = 'application/json') {
  return fetch(path, {
    method: 'POST',
    credentials: 'same-origin',
    headers: { 'Content-Type': type },
    body: type === 'application/json' ? JSON.stringify(body) : body,
  });
}

function scheduleRetry(ms = 2000) {
  clearTimeout(retryTimer);
  retryTimer = setTimeout(watch, ms);
}

function closePc() {
  if (pc) {
    pc.ontrack = pc.onconnectionstatechange = null;
    pc.close();
    pc = null;
  }
}

function waitIce(conn) {
  if (conn.iceGatheringState === 'complete') return Promise.resolve();
  return new Promise((resolve) => {
    const done = () => {
      if (conn.iceGatheringState === 'complete') {
        conn.removeEventListener('icegatheringstatechange', done);
        resolve();
      }
    };
    conn.addEventListener('icegatheringstatechange', done);
    setTimeout(resolve, 1500); // LAN host candidates arrive fast; don't wait forever
  });
}

async function watch() {
  clearTimeout(retryTimer);
  closePc();
  show('player');
  status('Connecting…');

  const conn = new RTCPeerConnection({ iceServers: [] });
  pc = conn;
  conn.addTransceiver('video', { direction: 'recvonly' });
  conn.addTransceiver('audio', { direction: 'recvonly' });

  const stream = new MediaStream();
  video.srcObject = stream;
  conn.ontrack = (e) => {
    // Movies don't need ultra-low latency: a ~0.4 s playout buffer absorbs Wi-Fi jitter
    // and makes motion smooth.
    try { e.receiver.jitterBufferTarget = 400; } catch (_) {}
    try { e.receiver.playoutDelayHint = 0.4; } catch (_) {}
    stream.addTrack(e.track);
    startPlayback();
  };
  conn.onconnectionstatechange = () => {
    if (pc !== conn) return;
    const s = conn.connectionState;
    if (s === 'connected') status('Live');
    else if (s === 'failed' || s === 'closed') {
      status('Connection lost. Reconnecting…');
      scheduleRetry();
    } else if (s === 'disconnected') {
      status('Reconnecting…');
      // The Mac stopped casting or Wi-Fi dropped: start over if it doesn't recover quickly.
      setTimeout(() => {
        if (pc === conn && conn.connectionState !== 'connected') scheduleRetry(0);
      }, 4000);
    }
  };

  try {
    await conn.setLocalDescription(await conn.createOffer());
    await waitIce(conn);
    const res = await post('/api/watch', conn.localDescription.sdp, 'application/sdp');
    if (pc !== conn) return;
    if (res.status === 401) {
      closePc();
      return showJoin('Session expired. Enter the PIN again.');
    }
    if (res.status === 409) {
      status('The computer is not casting right now. Waiting…');
      return scheduleRetry(3000);
    }
    if (!res.ok) throw new Error('watch ' + res.status);
    await conn.setRemoteDescription({ type: 'answer', sdp: await res.text() });
  } catch (err) {
    if (pc !== conn) return;
    status('Cannot reach the computer. Retrying…');
    scheduleRetry(3000);
  }
}

// Try to play WITH sound; browsers may block that until the user taps, so fall back to
// muted playback plus a big "turn on sound" button.
async function startPlayback() {
  if (!video.muted) {
    try { await video.play(); return; } catch (_) {}
  }
  video.muted = false;
  try {
    await video.play();
    $('soundPrompt').hidden = true;
  } catch (_) {
    video.muted = true;
    video.play().catch(() => {});
    $('soundPrompt').hidden = false;
  }
  updateMuteButton();
}

function updateMuteButton() {
  $('unmute').textContent = video.muted ? '🔊 Sound on' : '🔇 Mute';
}

$('soundPrompt').addEventListener('click', () => {
  video.muted = false;
  video.play().catch(() => {});
  $('soundPrompt').hidden = true;
  updateMuteButton();
});

function showJoin(msg = '') {
  show('join');
  $('joinMsg').textContent = msg;
  $('pin').value = '';
  $('pin').focus();
}

async function join(pin, token = '') {
  $('joinMsg').textContent = 'Checking…';
  try {
    const res = await post('/api/join', { pin, token });
    if (res.ok) return watch();
    if (res.status === 409) showJoin('The computer is not casting right now.');
    else if (res.status === 429) showJoin('Too many wrong tries. Wait a few minutes.');
    else if (res.status === 503) showJoin('Too many viewers are connected.');
    else showJoin('Wrong or expired PIN.');
  } catch {
    showJoin('Cannot reach the computer.');
  }
}

$('joinForm').addEventListener('submit', (e) => {
  e.preventDefault();
  join($('pin').value.trim());
});

// A scanned QR code opens "/#pin=123456". The fragment never reaches the server; read it
// once and remove it from the address bar and history.
const qrToken = (location.hash.match(/[#&]t=([0-9a-f]{32})/) || [])[1];
const qrPin = (location.hash.match(/pin=(\d{6})/) || [])[1];
if (location.hash) history.replaceState(null, '', location.pathname);

$('unmute').addEventListener('click', () => {
  video.muted = !video.muted;
  $('soundPrompt').hidden = true;
  updateMuteButton();
  video.play().catch(() => {});
});

$('fs').addEventListener('click', () => {
  const el = $('player');
  if (document.fullscreenElement) document.exitFullscreen();
  else if (el.requestFullscreen) el.requestFullscreen().catch(() => {});
  else if (video.webkitEnterFullscreen) video.webkitEnterFullscreen(); // iPhone
});

// Auto-hide the overlay while watching.
function poke() {
  $('overlay').classList.remove('hidden');
  clearTimeout(hideTimer);
  hideTimer = setTimeout(() => $('overlay').classList.add('hidden'), 3500);
}
['pointermove', 'pointerdown', 'keydown'].forEach((ev) => document.addEventListener(ev, poke));

(async () => {
  try {
    const res = await fetch('/api/me', { credentials: 'same-origin' });
    if (res.ok) watch();
    else if (qrToken) join('', qrToken);
    else if (qrPin) join(qrPin);
    else showJoin();
  } catch {
    showJoin('Cannot reach the computer.');
  }
  poke();
})();
