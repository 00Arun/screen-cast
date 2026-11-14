import { api, apiBase, CastError, Emitter, iceCandidateInit, openEvents, type EventStream } from './client.js';
import { KEY_RE, PIN_RE, ROOM_RE, type Signal } from './protocol.js';

export interface CastViewerOptions {
  /** Base URL of the signaling server, e.g. `/cast`. */
  server: string;
  /** Video element to play into (optional: listen to the `stream` event instead). */
  video?: HTMLVideoElement;
  /** STUN/TURN servers; must match what the host uses for cross-network casts. Default `[]`. */
  iceServers?: RTCIceServer[];
  /** Playout buffer in ms. Movies don't need ultra-low latency; ~400 ms smooths Wi-Fi jitter. */
  bufferMs?: number;
}

export type ViewerStatus =
  | 'idle'
  | 'joining'
  | 'waiting' // joined, host not sending yet
  | 'connecting'
  | 'live'
  | 'reconnecting'
  | 'ended' // host stopped / room closed
  | 'kicked';

export interface JoinOptions {
  /** 6-character room code. */
  room: string;
  /** 6-digit PIN, or `key` from a join link. */
  pin?: string;
  key?: string;
}

export interface CastViewerEvents extends Record<string, unknown> {
  status: ViewerStatus;
  stream: MediaStream;
  error: Error;
}

/**
 * Watches a cast. The viewer only ever receives an encrypted media stream.
 *
 *   const viewer = new CastViewer({ server: '/cast', video: document.querySelector('video')! });
 *   if (!(await viewer.joinFromUrl())) await viewer.join({ room: 'ABC234', pin: '482913' });
 */
export class CastViewer extends Emitter<CastViewerEvents> {
  readonly #base: string;
  readonly #video?: HTMLVideoElement;
  readonly #ice: RTCIceServer[];
  readonly #buffer: number;
  #room = '';
  #token = '';
  #events: EventStream | null = null;
  #pc: RTCPeerConnection | null = null;
  #pending: RTCIceCandidateInit[] = [];
  #status: ViewerStatus = 'idle';

  constructor(options: CastViewerOptions) {
    super();
    this.#base = apiBase(options.server);
    this.#video = options.video;
    this.#ice = options.iceServers ?? [];
    this.#buffer = options.bufferMs ?? 400;
  }

  get status(): ViewerStatus {
    return this.#status;
  }

  /** Joins with a room code + PIN (or join-link key). Throws CastError on a wrong PIN etc. */
  async join({ room, pin, key }: JoinOptions): Promise<void> {
    room = room.trim().toUpperCase();
    if (!ROOM_RE.test(room)) throw new CastError('Invalid room code', 400);
    if (!(pin && PIN_RE.test(pin.trim())) && !(key && KEY_RE.test(key))) throw new CastError('Invalid PIN', 400);
    this.leave();
    this.#set('joining');
    let r: { viewer: string; token: string };
    try {
      r = await api(`${this.#base}/rooms/${room}/join`, { method: 'POST', body: { pin: pin?.trim(), key } });
    } catch (e) {
      this.#set('idle');
      throw e;
    }
    this.#room = room;
    this.#token = r.token;
    this.#events = openEvents(
      `${this.#base}/rooms/${room}/events`,
      r.token,
      (event, data) => this.#onEvent(event, data as Record<string, unknown>),
      (state) => {
        if (state === 'gone') this.#end('ended');
        else if (state === 'reconnecting' && this.#status !== 'live') this.#set('reconnecting');
      },
    );
  }

  /**
   * Joins from a link made by `session.joinUrl()` (`#room=...&key=...`). The secret is
   * removed from the address bar right away. Returns false if the URL has no join info.
   */
  async joinFromUrl(hash: string = typeof location !== 'undefined' ? location.hash : ''): Promise<boolean> {
    const params = new URLSearchParams(hash.replace(/^#/, ''));
    const room = params.get('room');
    const key = params.get('key');
    if (typeof history !== 'undefined' && hash && typeof location !== 'undefined' && hash === location.hash) {
      history.replaceState(null, '', location.pathname + location.search);
    }
    if (!room || !key) return false;
    await this.join({ room, key });
    return true;
  }

  leave(): void {
    this.#events?.close();
    this.#events = null;
    this.#closePc();
    this.#token = '';
    this.#room = '';
    if (this.#status !== 'idle') this.#set('idle');
  }

  // ---------------------------------------------------------------------------------------

  #set(s: ViewerStatus) {
    if (this.#status === s) return;
    this.#status = s;
    this.emit('status', s);
  }

  #end(s: 'ended' | 'kicked') {
    this.#events?.close();
    this.#events = null;
    this.#closePc();
    this.#token = '';
    this.#set(s);
  }

  #closePc() {
    if (!this.#pc) return;
    this.#pc.ontrack = this.#pc.onconnectionstatechange = this.#pc.onicecandidate = null;
    this.#pc.close();
    this.#pc = null;
    this.#pending = [];
  }

  #onEvent(event: string, d: Record<string, unknown>) {
    switch (event) {
      case 'hello':
        if (this.#status !== 'live') this.#set(d.hostOnline ? 'connecting' : 'waiting');
        break;
      case 'host-left':
        this.#closePc();
        this.#set('waiting');
        break;
      case 'kicked':
        this.#end('kicked');
        break;
      case 'closed':
        this.#end('ended');
        break;
      case 'signal':
        void this.#onSignal(d.data as Signal);
        break;
    }
  }

  async #send(data: Signal) {
    if (!this.#token) return;
    try {
      await api(`${this.#base}/rooms/${this.#room}/signal`, { method: 'POST', token: this.#token, body: { data } });
    } catch {
      /* host offline */
    }
  }

  async #onSignal(data: Signal) {
    try {
      if (data.type === 'offer') {
        this.#closePc();
        const pc = new RTCPeerConnection({ iceServers: this.#ice });
        this.#pc = pc;
        const stream = new MediaStream();
        pc.ontrack = (e) => {
          const r = e.receiver as RTCRtpReceiver & { jitterBufferTarget?: number; playoutDelayHint?: number };
          try {
            r.jitterBufferTarget = this.#buffer;
          } catch {
            /* unsupported */
          }
          try {
            r.playoutDelayHint = this.#buffer / 1000;
          } catch {
            /* unsupported */
          }
          stream.addTrack(e.track);
          if (this.#video && this.#video.srcObject !== stream) {
            this.#video.srcObject = stream;
            this.#video.play().catch(() => {
              /* autoplay with sound blocked: caller shows an unmute button */
            });
          }
          this.emit('stream', stream);
        };
        pc.onicecandidate = (e) => {
          if (e.candidate && e.candidate.candidate) void this.#send({ type: 'candidate', candidate: iceCandidateInit(e.candidate) });
        };
        pc.onconnectionstatechange = () => {
          if (this.#pc !== pc) return;
          if (pc.connectionState === 'connected') this.#set('live');
          else if (pc.connectionState === 'failed' || pc.connectionState === 'disconnected') this.#set('reconnecting');
        };
        await pc.setRemoteDescription({ type: 'offer', sdp: data.sdp });
        await pc.setLocalDescription(await pc.createAnswer());
        await this.#send({ type: 'answer', sdp: pc.localDescription!.sdp });
        for (const c of this.#pending.splice(0)) await pc.addIceCandidate(c);
        this.#set('connecting');
      } else if (data.type === 'candidate') {
        if (this.#pc?.remoteDescription) await this.#pc.addIceCandidate(data.candidate);
        else this.#pending.push(data.candidate);
      } else if (data.type === 'bye') {
        this.#closePc();
        this.#set('waiting');
      }
    } catch (e) {
      this.emit('error', e instanceof Error ? e : new Error(String(e)));
    }
  }
}
