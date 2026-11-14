import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { createCastServer } from '../dist/server.js';
import { sanitizeSignal } from '../dist/protocol.js';

let cast;
let server;
let base;

before(async () => {
  cast = createCastServer({ path: '/cast', maxViewers: 3, maxRooms: 50, allowedOrigins: ['https://app.example'] });
  server = await cast.listen(0, '127.0.0.1');
  base = `http://127.0.0.1:${server.address().port}/cast`;
});

after(() => {
  cast.close();
  server.close();
});

const post = (path, body, token, headers = {}) =>
  fetch(base + path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...(token ? { Authorization: `Bearer ${token}` } : {}), ...headers },
    body: JSON.stringify(body ?? {}),
  });

/** Reads server-sent events from a fetch stream. */
function events(path, token) {
  const ctrl = new AbortController();
  const queue = [];
  const waiters = [];
  const ready = fetch(base + path, { headers: { Authorization: `Bearer ${token}` }, signal: ctrl.signal }).then(async (res) => {
    if (!res.ok) return res.status;
    (async () => {
      const dec = new TextDecoder();
      let buf = '';
      try {
        for await (const chunk of res.body) {
          buf += dec.decode(chunk);
          let i;
          while ((i = buf.indexOf('\n\n')) >= 0) {
            const block = buf.slice(0, i);
            buf = buf.slice(i + 2);
            const ev = /event: (.*)/.exec(block)?.[1];
            const data = /data: (.*)/.exec(block)?.[1];
            if (!ev) continue;
            const item = { event: ev, data: JSON.parse(data) };
            const w = waiters.shift();
            if (w) w(item);
            else queue.push(item);
          }
        }
      } catch {}
    })();
    return 200;
  });
  return {
    ready,
    next: (ms = 3000) =>
      new Promise((resolve, reject) => {
        if (queue.length) return resolve(queue.shift());
        const t = setTimeout(() => reject(new Error('timeout waiting for event')), ms);
        waiters.push((v) => {
          clearTimeout(t);
          resolve(v);
        });
      }),
    close: () => ctrl.abort(),
  };
}

async function newRoom() {
  const res = await post('/rooms');
  assert.equal(res.status, 201);
  return res.json();
}

test('room creation returns secrets; room code and PIN format', async () => {
  const r = await newRoom();
  assert.match(r.room, /^[A-Z2-9]{6}$/);
  assert.match(r.pin, /^\d{6}$/);
  assert.match(r.key, /^[0-9a-f]{32}$/);
  assert.match(r.hostToken, /^[0-9a-f]{64}$/);
});

test('join with PIN or key', async () => {
  const r = await newRoom();
  assert.equal((await post(`/rooms/${r.room}/join`, { key: r.key })).status, 200);
  const ok = await post(`/rooms/${r.room}/join`, { pin: r.pin });
  assert.equal(ok.status, 200);
  const j = await ok.json();
  assert.match(j.token, /^[0-9a-f]{64}$/);

});

test('per-IP lock-out after 5 wrong PINs', async () => {
  // Own server so the lock-out doesn't affect the other tests.
  const c = createCastServer();
  const s = await c.listen(0, '127.0.0.1');
  const b = `http://127.0.0.1:${s.address().port}/cast`;
  const join = (room, pin) =>
    fetch(`${b}/rooms/${room}/join`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ pin }) });
  const r2 = await (await fetch(`${b}/rooms`, { method: 'POST' })).json();
  const wrong = r2.pin === '000000' ? '111111' : '000000';
  for (let i = 0; i < 5; i++) assert.equal((await join(r2.room, wrong)).status, 401);
  // Locked out now, even with the right PIN.
  assert.equal((await join(r2.room, r2.pin)).status, 429);
  c.close();
  s.close();
});

test('room-wide wrong-PIN budget rotates the PIN (spread over many addresses)', async () => {
  const c = createCastServer({ trustProxy: true });
  const s = await c.listen(0, '127.0.0.1');
  const b = `http://127.0.0.1:${s.address().port}/cast`;
  const r = await (await fetch(`${b}/rooms`, { method: 'POST' })).json();
  const host = (() => {
    const ctrl = new AbortController();
    return { ctrl, res: fetch(`${b}/rooms/${r.room}/events`, { headers: { Authorization: `Bearer ${r.hostToken}` }, signal: ctrl.signal }) };
  })();
  await host.res;
  for (let i = 0; i < 20; i++) {
    const res = await fetch(`${b}/rooms/${r.room}/join`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', 'X-Forwarded-For': `10.0.${i}.1` },
      body: JSON.stringify({ pin: r.pin === '000000' ? '111111' : '000000' }),
    });
    assert.equal(res.status, 401);
  }
  // The old PIN no longer works.
  const old = await fetch(`${b}/rooms/${r.room}/join`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', 'X-Forwarded-For': '10.9.9.9' },
    body: JSON.stringify({ pin: r.pin }),
  });
  assert.equal(old.status, 401);
  host.ctrl.abort();
  c.close();
  s.close();
});

test('event streams and signal relay with strict validation and roles', async () => {
  const r = await newRoom();
  const host = events(`/rooms/${r.room}/events`, r.hostToken);
  assert.equal(await host.ready, 200);
  assert.equal((await host.next()).event, 'ready');

  const j = await (await post(`/rooms/${r.room}/join`, { pin: r.pin })).json();
  const viewer = events(`/rooms/${r.room}/events`, j.token);
  assert.equal(await viewer.ready, 200);
  assert.deepEqual((await viewer.next()).data, { hostOnline: true });
  const joined = await host.next();
  assert.equal(joined.event, 'viewer-joined');
  assert.equal(joined.data.id, j.viewer);

  // host -> viewer offer (extra fields stripped)
  const sdp = 'v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\n';
  assert.equal((await post(`/rooms/${r.room}/signal`, { to: j.viewer, data: { type: 'offer', sdp, evil: '<script>' } }, r.hostToken)).status, 204);
  const got = await viewer.next();
  assert.deepEqual(got.data, { data: { type: 'offer', sdp } });

  // viewer -> host answer
  assert.equal((await post(`/rooms/${r.room}/signal`, { data: { type: 'answer', sdp } }, j.token)).status, 204);
  const ans = await host.next();
  assert.equal(ans.data.from, j.viewer);

  // role rules: viewers can't send offers or bye; host can't send answers
  assert.equal((await post(`/rooms/${r.room}/signal`, { data: { type: 'offer', sdp } }, j.token)).status, 400);
  assert.equal((await post(`/rooms/${r.room}/signal`, { to: j.viewer, data: { type: 'answer', sdp } }, r.hostToken)).status, 400);
  // malformed signals
  assert.equal((await post(`/rooms/${r.room}/signal`, { data: { type: 'answer', sdp: 'not sdp' } }, j.token)).status, 400);
  assert.equal((await post(`/rooms/${r.room}/signal`, { data: { type: 'hack' } }, j.token)).status, 400);
  // no / wrong token
  assert.equal((await post(`/rooms/${r.room}/signal`, { data: { type: 'answer', sdp } })).status, 401);
  assert.equal((await post(`/rooms/${r.room}/signal`, { data: { type: 'answer', sdp } }, 'f'.repeat(64))).status, 401);
  // viewers can't use host actions
  assert.equal((await post(`/rooms/${r.room}/pin`, {}, j.token)).status, 401);

  // host rotates PIN: viewer is kicked, host notified
  const rot = await (await post(`/rooms/${r.room}/pin`, {}, r.hostToken)).json();
  assert.notEqual(rot.key, r.key);
  assert.equal((await viewer.next()).event, 'kicked');
  host.close();
  viewer.close();
});

test('session token is bound to the IP it was issued to', async () => {
  const c = createCastServer({ trustProxy: true });
  const s = await c.listen(0, '127.0.0.1');
  const b = `http://127.0.0.1:${s.address().port}/cast`;
  const r = await (await fetch(`${b}/rooms`, { method: 'POST' })).json();
  const j = await (await fetch(`${b}/rooms/${r.room}/join`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', 'X-Forwarded-For': '10.1.1.1' },
    body: JSON.stringify({ pin: r.pin }),
  })).json();
  const stolen = await fetch(`${b}/rooms/${r.room}/events`, {
    headers: { Authorization: `Bearer ${j.token}`, 'X-Forwarded-For': '10.2.2.2' },
  });
  assert.equal(stolen.status, 401);
  c.close();
  s.close();
});

test('cross-site requests are refused; allowed origins get CORS', async () => {
  const evil = await post('/rooms', {}, null, { Origin: 'https://evil.example' });
  assert.equal(evil.status, 403);
  const good = await post('/rooms', {}, null, { Origin: 'https://app.example' });
  assert.equal(good.status, 201);
  assert.equal(good.headers.get('access-control-allow-origin'), 'https://app.example');
});

test('viewer cap, unknown rooms, size limit', async () => {
  const r = await newRoom();
  for (let i = 0; i < 3; i++) assert.equal((await post(`/rooms/${r.room}/join`, { pin: r.pin })).status, 200);
  assert.equal((await post(`/rooms/${r.room}/join`, { pin: r.pin })).status, 503);
  assert.equal((await post('/rooms/ZZZZZZ/join', { pin: '123456' })).status, 404);
  assert.equal((await post('/rooms/../../etc/join', { pin: '123456' })).status, 404);
  const big = await post(`/rooms/${r.room}/join`, { pin: 'x'.repeat(100_000) });
  assert.equal(big.status, 413);
});

test('host closing a room ends it for viewers', async () => {
  const r = await newRoom();
  const j = await (await post(`/rooms/${r.room}/join`, { pin: r.pin })).json();
  const viewer = events(`/rooms/${r.room}/events`, j.token);
  await viewer.ready;
  await viewer.next(); // hello
  const del = await fetch(`${base}/rooms/${r.room}`, { method: 'DELETE', headers: { Authorization: `Bearer ${r.hostToken}` } });
  assert.equal(del.status, 200);
  assert.equal((await viewer.next()).event, 'closed');
  assert.equal((await post(`/rooms/${r.room}/join`, { pin: r.pin })).status, 404);
  viewer.close();
});

test('sanitizeSignal', () => {
  assert.equal(sanitizeSignal(null), null);
  assert.equal(sanitizeSignal({ type: 'offer', sdp: 'x' }), null);
  assert.equal(sanitizeSignal({ type: 'offer', sdp: 'v=0' + 'a'.repeat(40_000) }), null);
  assert.deepEqual(sanitizeSignal({ type: 'candidate', candidate: { candidate: 'c', sdpMid: '0', sdpMLineIndex: 0, x: 1 } }), {
    type: 'candidate',
    candidate: { candidate: 'c', sdpMid: '0', sdpMLineIndex: 0 },
  });
  assert.equal(sanitizeSignal({ type: 'candidate', candidate: { candidate: 'c', sdpMLineIndex: -1 } }), null);
  assert.deepEqual(sanitizeSignal({ type: 'bye', extra: 1 }), { type: 'bye' });
});
