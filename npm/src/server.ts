/**
 * screen-cast signaling server for Node.js (no dependencies).
 *
 * It only introduces hosts and viewers to each other (WebRTC offer/answer/ICE). Video and
 * audio flow directly between browsers, encrypted (DTLS-SRTP); they never pass through here.
 *
 *   import { createCastServer } from 'secure-screen-cast/server';
 *   const cast = createCastServer({ path: '/cast' });
 *   app.use(cast.middleware);            // Express / Connect
 *   // or: http.createServer((req, res) => cast.handle(req, res).then(done => done || next()))
 *   // or: await cast.listen(8080);
 */

import { createServer, type IncomingMessage, type Server, type ServerResponse } from 'node:http';
import { randomBytes, randomInt, timingSafeEqual } from 'node:crypto';

import { KEY_RE, LIMITS, PIN_RE, ROOM_ALPHABET, ROOM_RE, sanitizeSignal } from './protocol.js';

export interface CastServerOptions {
  /** URL prefix for all endpoints. Default `/cast`. */
  path?: string;
  /** Maximum simultaneous rooms (casts). Default 100. */
  maxRooms?: number;
  /** Maximum viewers per room. Default 12. */
  maxViewers?: number;
  /**
   * Extra origins (e.g. `https://app.example.com`) allowed to call the API from a browser.
   * Same-origin requests are always allowed. Default: none.
   */
  allowedOrigins?: string[];
  /** Use the first `X-Forwarded-For` address as the client IP (only behind a trusted proxy). */
  trustProxy?: boolean;
  /**
   * Decide who may start a cast (e.g. check your app's login session). Default: anyone,
   * subject to rate limits.
   */
  authorizeHost?: (req: IncomingMessage) => boolean | Promise<boolean>;
  /** Maximum lifetime of a room in ms. Default 6 hours. */
  roomTtlMs?: number;
}

export interface CastServer {
  /** Handles the request if it targets the cast API. Resolves `true` when handled. */
  handle(req: IncomingMessage, res: ServerResponse): Promise<boolean>;
  /** Connect/Express-style middleware. */
  middleware(req: IncomingMessage, res: ServerResponse, next: (err?: unknown) => void): void;
  /** Starts a standalone HTTP server that serves only the cast API. */
  listen(port: number, host?: string): Promise<Server>;
  /** Number of active rooms. */
  roomCount(): number;
  /** Ends every room and stops timers. */
  close(): void;
}

// ---------------------------------------------------------------------------------------------

const MAX_PIN_FAILS = 5; // per client IP
const PIN_LOCK_MS = 5 * 60_000;
const ROOM_FAIL_BUDGET = 20; // per room, from anywhere: then the PIN and key rotate
const ROOM_FAIL_WINDOW_MS = 10 * 60_000;
const ROOMS_PER_IP = 10; // room creations per window
const ROOM_CREATE_WINDOW_MS = 10 * 60_000;
const HOST_GRACE_MS = 60_000; // keep a room this long after the host disconnects
const VIEWER_GRACE_MS = 2 * 60_000;
const HEARTBEAT_MS = 15_000;

interface Stream {
  res: ServerResponse;
  id: number;
}

interface Viewer {
  id: string;
  token: string;
  ip: string;
  stream: Stream | null;
  seen: number;
}

interface Room {
  id: string;
  hostToken: string;
  pin: string;
  key: string;
  created: number;
  host: Stream | null;
  hostSeen: number;
  viewers: Map<string, Viewer>;
  fails: number[];
}

const hex = (bytes: number) => randomBytes(bytes).toString('hex');
const newPin = () => String(randomInt(0, 1_000_000)).padStart(6, '0');
const newRoomId = () => Array.from({ length: 6 }, () => ROOM_ALPHABET[randomInt(0, ROOM_ALPHABET.length)]).join('');

function safeEqual(a: string, b: string): boolean {
  const x = Buffer.from(a);
  const y = Buffer.from(b);
  return x.length === y.length && timingSafeEqual(x, y);
}

function send(res: ServerResponse, status: number, body: unknown, extra: Record<string, string> = {}) {
  if (res.headersSent) return;
  const json = JSON.stringify(body);
  res.writeHead(status, {
    'Content-Type': 'application/json; charset=utf-8',
    'Content-Length': Buffer.byteLength(json),
    'Cache-Control': 'no-store',
    'X-Content-Type-Options': 'nosniff',
    ...extra,
  });
  res.end(json);
}

function readJson(req: IncomingMessage): Promise<Record<string, unknown>> {
  return new Promise((resolve, reject) => {
    let size = 0;
    let tooLarge = false;
    const chunks: Buffer[] = [];
    req.on('data', (c: Buffer) => {
      size += c.length;
      if (tooLarge) return; // drain and discard the rest
      if (size > LIMITS.body) {
        tooLarge = true;
        chunks.length = 0;
        reject(new Error('too large'));
        return;
      }
      chunks.push(c);
    });
    req.on('end', () => {
      if (tooLarge) return;
      if (size === 0) return resolve({});
      try {
        const v = JSON.parse(Buffer.concat(chunks).toString('utf8'));
        resolve(v && typeof v === 'object' && !Array.isArray(v) ? v : {});
      } catch {
        reject(new Error('bad json'));
      }
    });
    req.on('error', reject);
  });
}

function sse(stream: Stream | null, event: string, data: unknown) {
  if (!stream) return;
  stream.res.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
}

// ---------------------------------------------------------------------------------------------

export function createCastServer(options: CastServerOptions = {}): CastServer {
  const base = (options.path ?? '/cast').replace(/\/+$/, '');
  const maxRooms = options.maxRooms ?? 100;
  const maxViewers = options.maxViewers ?? 12;
  const roomTtl = options.roomTtlMs ?? 6 * 60 * 60_000;
  const allowed = new Set((options.allowedOrigins ?? []).map((o) => o.replace(/\/+$/, '').toLowerCase()));

  const rooms = new Map<string, Room>();
  const fails = new Map<string, { n: number; since: number }>();
  const creations = new Map<string, number[]>();
  let streamSeq = 0;

  const clientIp = (req: IncomingMessage) => {
    if (options.trustProxy) {
      const fwd = req.headers['x-forwarded-for'];
      const first = (Array.isArray(fwd) ? fwd[0] : fwd)?.split(',')[0]?.trim();
      if (first) return first;
    }
    return req.socket.remoteAddress ?? 'unknown';
  };

  /** Browser requests must come from this site or an allowed origin (blocks CSRF). */
  const originCheck = (req: IncomingMessage): { ok: boolean; cors: Record<string, string> } => {
    const origin = req.headers.origin;
    if (!origin) return { ok: true, cors: {} }; // non-browser client: tokens still required
    const o = origin.toLowerCase();
    const host = (req.headers.host ?? '').toLowerCase();
    let sameOrigin = false;
    try {
      sameOrigin = new URL(o).host === host;
    } catch {
      return { ok: false, cors: {} };
    }
    if (sameOrigin) return { ok: true, cors: {} };
    if (allowed.has(o)) {
      return {
        ok: true,
        cors: {
          'Access-Control-Allow-Origin': origin,
          'Access-Control-Allow-Headers': 'Authorization, Content-Type',
          'Access-Control-Allow-Methods': 'GET, POST, DELETE, OPTIONS',
          'Access-Control-Max-Age': '600',
          Vary: 'Origin',
        },
      };
    }
    return { ok: false, cors: {} };
  };

  const bearer = (req: IncomingMessage) => {
    const h = req.headers.authorization ?? '';
    return h.startsWith('Bearer ') ? h.slice(7).trim() : '';
  };

  /** Who is calling: the room's host, one of its viewers, or nobody. */
  const whoIs = (room: Room, req: IncomingMessage): { role: 'host' } | { role: 'viewer'; viewer: Viewer } | null => {
    const token = bearer(req);
    if (!token) return null;
    if (safeEqual(token, room.hostToken)) return { role: 'host' };
    const ip = clientIp(req);
    for (const v of room.viewers.values()) {
      if (safeEqual(token, v.token) && v.ip === ip) return { role: 'viewer', viewer: v };
    }
    return null;
  };

  const kickAll = (room: Room) => {
    for (const v of room.viewers.values()) {
      sse(v.stream, 'kicked', {});
      v.stream?.res.end();
      sse(room.host, 'viewer-left', { id: v.id });
    }
    room.viewers.clear();
  };

  const rotate = (room: Room) => {
    room.pin = newPin();
    room.key = hex(16);
    room.fails = [];
    kickAll(room);
    sse(room.host, 'pin', { pin: room.pin, key: room.key });
  };

  const endRoom = (room: Room) => {
    for (const v of room.viewers.values()) {
      sse(v.stream, 'closed', {});
      v.stream?.res.end();
    }
    room.host?.res.end();
    rooms.delete(room.id);
  };

  const sweep = setInterval(() => {
    const now = Date.now();
    for (const room of rooms.values()) {
      if (now - room.created > roomTtl || (!room.host && now - room.hostSeen > HOST_GRACE_MS)) {
        endRoom(room);
        continue;
      }
      for (const v of room.viewers.values()) {
        if (!v.stream && now - v.seen > VIEWER_GRACE_MS) room.viewers.delete(v.id);
      }
    }
    for (const [ip, f] of fails) if (now - f.since > PIN_LOCK_MS) fails.delete(ip);
    for (const [ip, list] of creations) {
      const recent = list.filter((t) => now - t < ROOM_CREATE_WINDOW_MS);
      if (recent.length) creations.set(ip, recent);
      else creations.delete(ip);
    }
  }, 10_000);
  sweep.unref?.();

  const heartbeat = setInterval(() => {
    for (const room of rooms.values()) {
      room.host?.res.write(': ping\n\n');
      for (const v of room.viewers.values()) v.stream?.res.write(': ping\n\n');
    }
  }, HEARTBEAT_MS);
  heartbeat.unref?.();

  // ----- endpoints -----

  async function createRoom(req: IncomingMessage, res: ServerResponse, cors: Record<string, string>) {
    if (options.authorizeHost && !(await options.authorizeHost(req))) {
      return send(res, 403, { error: 'not allowed to cast' }, cors);
    }
    const ip = clientIp(req);
    const now = Date.now();
    const recent = (creations.get(ip) ?? []).filter((t) => now - t < ROOM_CREATE_WINDOW_MS);
    if (recent.length >= ROOMS_PER_IP) return send(res, 429, { error: 'too many casts started' }, cors);
    if (rooms.size >= maxRooms) return send(res, 503, { error: 'server busy' }, cors);
    let id = newRoomId();
    while (rooms.has(id)) id = newRoomId();
    const room: Room = {
      id,
      hostToken: hex(32),
      pin: newPin(),
      key: hex(16),
      created: now,
      host: null,
      hostSeen: now,
      viewers: new Map(),
      fails: [],
    };
    rooms.set(id, room);
    creations.set(ip, [...recent, now]);
    send(res, 201, { room: id, hostToken: room.hostToken, pin: room.pin, key: room.key, maxViewers }, cors);
  }

  async function join(room: Room, req: IncomingMessage, res: ServerResponse, cors: Record<string, string>) {
    const ip = clientIp(req);
    const now = Date.now();
    const f = fails.get(ip);
    if (f && f.n >= MAX_PIN_FAILS && now - f.since < PIN_LOCK_MS) {
      return send(res, 429, { error: 'too many attempts, wait a few minutes' }, cors);
    }
    const body = await readJson(req);
    const pin = typeof body.pin === 'string' ? body.pin.trim() : '';
    const key = typeof body.key === 'string' ? body.key.trim() : '';
    const ok = (PIN_RE.test(pin) && safeEqual(pin, room.pin)) || (KEY_RE.test(key) && safeEqual(key, room.key));
    if (!ok) {
      const cur = fails.get(ip);
      fails.set(ip, { n: (cur && now - cur.since < PIN_LOCK_MS ? cur.n : 0) + 1, since: now });
      room.fails = room.fails.filter((t) => now - t < ROOM_FAIL_WINDOW_MS);
      room.fails.push(now);
      if (room.fails.length >= ROOM_FAIL_BUDGET) rotate(room);
      return send(res, 401, { error: 'wrong PIN' }, cors);
    }
    fails.delete(ip);
    if (room.viewers.size >= maxViewers) return send(res, 503, { error: 'too many viewers' }, cors);
    const viewer: Viewer = { id: hex(6), token: hex(32), ip, stream: null, seen: now };
    room.viewers.set(viewer.id, viewer);
    send(res, 200, { viewer: viewer.id, token: viewer.token }, cors);
  }

  function events(room: Room, req: IncomingMessage, res: ServerResponse, cors: Record<string, string>) {
    const who = whoIs(room, req);
    if (!who) return send(res, 401, { error: 'not authorized' }, cors);
    res.writeHead(200, {
      'Content-Type': 'text/event-stream; charset=utf-8',
      'Cache-Control': 'no-store',
      Connection: 'keep-alive',
      'X-Accel-Buffering': 'no',
      'X-Content-Type-Options': 'nosniff',
      ...cors,
    });
    res.flushHeaders?.();
    const stream: Stream = { res, id: ++streamSeq };
    if (who.role === 'host') {
      room.host?.res.end(); // one host connection at a time
      room.host = stream;
      room.hostSeen = Date.now();
      const online = [...room.viewers.values()].filter((v) => v.stream).map((v) => v.id);
      sse(stream, 'ready', { room: room.id, pin: room.pin, key: room.key, viewers: online });
      for (const v of room.viewers.values()) sse(v.stream, 'hello', { hostOnline: true });
    } else {
      const v = who.viewer;
      v.stream?.res.end();
      v.stream = stream;
      v.seen = Date.now();
      sse(stream, 'hello', { hostOnline: !!room.host });
      sse(room.host, 'viewer-joined', { id: v.id });
    }
    req.on('close', () => {
      if (who.role === 'host') {
        if (room.host?.id === stream.id) {
          room.host = null;
          room.hostSeen = Date.now();
          for (const v of room.viewers.values()) sse(v.stream, 'host-left', {});
        }
      } else {
        const v = who.viewer;
        if (v.stream?.id === stream.id) {
          v.stream = null;
          v.seen = Date.now();
          sse(room.host, 'viewer-left', { id: v.id });
        }
      }
    });
  }

  async function signal(room: Room, req: IncomingMessage, res: ServerResponse, cors: Record<string, string>) {
    const who = whoIs(room, req);
    if (!who) return send(res, 401, { error: 'not authorized' }, cors);
    const body = await readJson(req);
    const data = sanitizeSignal(body.data);
    if (!data) return send(res, 400, { error: 'invalid signal' }, cors);
    if (who.role === 'host') {
      if (!['offer', 'candidate', 'bye'].includes(data.type)) return send(res, 400, { error: 'invalid signal' }, cors);
      const to = typeof body.to === 'string' ? room.viewers.get(body.to) : undefined;
      if (!to?.stream) return send(res, 404, { error: 'viewer offline' }, cors);
      sse(to.stream, 'signal', { data });
    } else {
      if (!['answer', 'candidate'].includes(data.type)) return send(res, 400, { error: 'invalid signal' }, cors);
      if (!room.host) return send(res, 409, { error: 'host offline' }, cors);
      sse(room.host, 'signal', { from: who.viewer.id, data });
    }
    res.writeHead(204, { 'Cache-Control': 'no-store', ...cors });
    res.end();
  }

  async function hostAction(
    room: Room,
    action: string,
    req: IncomingMessage,
    res: ServerResponse,
    cors: Record<string, string>,
  ) {
    const who = whoIs(room, req);
    if (who?.role !== 'host') return send(res, 401, { error: 'not authorized' }, cors);
    if (action === 'pin') {
      rotate(room);
      return send(res, 200, { pin: room.pin, key: room.key }, cors);
    }
    if (action === 'kick') {
      const body = await readJson(req);
      const v = typeof body.viewer === 'string' ? room.viewers.get(body.viewer) : undefined;
      if (v) {
        sse(v.stream, 'kicked', {});
        v.stream?.res.end();
        room.viewers.delete(v.id);
        sse(room.host, 'viewer-left', { id: v.id });
      }
      return send(res, 200, { ok: true }, cors);
    }
    // close
    endRoom(room);
    return send(res, 200, { ok: true }, cors);
  }

  async function handle(req: IncomingMessage, res: ServerResponse): Promise<boolean> {
    const url = new URL(req.url ?? '/', 'http://localhost');
    if (url.pathname !== base && !url.pathname.startsWith(`${base}/`)) return false;
    const { ok, cors } = originCheck(req);
    if (!ok) {
      send(res, 403, { error: 'origin not allowed' });
      return true;
    }
    const method = req.method ?? 'GET';
    if (method === 'OPTIONS') {
      res.writeHead(204, { 'Cache-Control': 'no-store', ...cors });
      res.end();
      return true;
    }
    try {
      const rest = url.pathname.slice(base.length).split('/').filter(Boolean);
      if (rest.length === 1 && rest[0] === 'rooms' && method === 'POST') {
        await createRoom(req, res, cors);
        return true;
      }
      if (rest[0] === 'rooms' && rest.length >= 2) {
        const id = rest[1].toUpperCase();
        const room = ROOM_RE.test(id) ? rooms.get(id) : undefined;
        if (!room) {
          send(res, 404, { error: 'no such cast' }, cors);
          return true;
        }
        const action = rest[2] ?? '';
        if (rest.length === 2 && method === 'DELETE') await hostAction(room, 'close', req, res, cors);
        else if (action === 'join' && method === 'POST') await join(room, req, res, cors);
        else if (action === 'events' && method === 'GET') events(room, req, res, cors);
        else if (action === 'signal' && method === 'POST') await signal(room, req, res, cors);
        else if ((action === 'pin' || action === 'kick') && method === 'POST') {
          await hostAction(room, action, req, res, cors);
        } else send(res, 404, { error: 'not found' }, cors);
        return true;
      }
      send(res, 404, { error: 'not found' }, cors);
    } catch (e) {
      const msg = e instanceof Error ? e.message : 'error';
      send(res, msg === 'too large' ? 413 : 400, { error: msg === 'too large' ? 'request too large' : 'bad request' }, {
        ...cors,
        Connection: 'close',
      });
    }
    return true;
  }

  return {
    handle,
    middleware(req, res, next) {
      handle(req, res).then((done) => (done ? undefined : next()), next);
    },
    listen(port, host) {
      const server = createServer((req, res) => {
        handle(req, res).then((done) => {
          if (!done) send(res, 404, { error: 'not found' });
        });
      });
      server.headersTimeout = 10_000;
      server.requestTimeout = 0; // event streams are long-lived
      return new Promise((resolve) => server.listen(port, host, () => resolve(server)));
    },
    roomCount: () => rooms.size,
    close() {
      clearInterval(sweep);
      clearInterval(heartbeat);
      for (const room of [...rooms.values()]) endRoom(room);
    },
  };
}
