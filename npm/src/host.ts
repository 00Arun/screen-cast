import { api, apiBase, Emitter, iceCandidateInit, openEvents, type EventStream } from './client.js';
import type { Signal } from './protocol.js';

/** What to cast. */
export type CastSource = 'screen' | MediaStream | HTMLVideoElement | HTMLCanvasElement;

export interface CastHostOptions {
  /** Base URL of the signaling server, e.g. `/cast` or `https://api.example.com/cast`. */
  server: string;
  /**
   * STUN/TURN servers. Default `[]`, which works on the same network (LAN). For viewers on
   * other networks add STUN, and TURN for strict NATs.
   */
  iceServers?: RTCIceServer[];
  /** Video bitrate cap in bits/s. Default 6 Mbps. */
  maxBitrate?: number;
  /** Frame rate cap. Default 30. */
  maxFramerate?: number;
}

export interface StartOptions {
  source?: CastSource;
  /** Capture audio with the screen/tab (where the browser supports it). Default true. */
  audio?: boolean;
}

export interface CastSession {
  /** 6-character room code viewers type in (with the PIN). */
  room: string;
  /** 6-digit PIN. */
  pin: string;
  /** Link for viewers: opens your viewer page and joins without typing anything. */
  joinUrl(viewerPageUrl: string): string;
}

export interface CastHostEvents extends Record<string, unknown> {
  /** Number of connected viewers changed. */
  viewers: number;
  /** PIN (and join link secret) changed: share the new values. */
  pin: { pin: string; room: string };
  /** The cast ended (stop(), the shared screen was closed, or the room expired). */
  ended: void;
  error: Error;
}

type Peer = { pc: RTCPeerConnection; pending: RTCIceCandidateInit[] };

/**
 * Casts a screen, tab, `<video>`, `<canvas>` or MediaStream to viewers.
 *
 *   const host = new CastHost({ server: '/cast' });
 *   const session = await host.start({ source: 'screen' });
 *   showToUser(session.room, session.pin, session.joinUrl('https://example.com/watch'));
 */
export class CastHost extends Emitter<CastHostEvents> {
  readonly #base: string;
  readonly #opts: Required<Omit<CastHostOptions, 'server'>>;
  #stream: MediaStream | null = null;
  #ownsStream = false;
  #room = '';
  #token = '';
  #pin = '';
  #key = '';
  #events: EventStream | null = null;
  #peers = new Map<string, Peer>();

  constructor(options: CastHostOptions) {
    super();
    this.#base = apiBase(options.server);
    this.#opts = {
      iceServers: options.iceServers ?? [],
      maxBitrate: options.maxBitrate ?? 6_000_000,
      maxFramerate: options.maxFramerate ?? 30,
    };
  }

  get active(): boolean {
    return !!this.#token;
  }

  get viewerCount(): number {
    return [...this.#peers.values()].filter((p) => p.pc.connectionState === 'connected').length;
  }

  async start({ source = 'screen', audio = true }: StartOptions = {}): Promise<CastSession> {
    if (this.active) await this.stop();
    const stream = await this.#capture(source, audio);
    try {
      const r = await api<{ room: string; hostToken: string; pin: string; key: string }>(`${this.#base}/rooms`, {
        method: 'POST',
        body: {},
      });
      this.#room = r.room;
      this.#token = r.hostToken;
      this.#pin = r.pin;
      this.#key = r.key;
    } catch (e) {
      if (this.#ownsStream) stream.getTracks().forEach((t) => t.stop());
      throw e;
    }
    this.#stream = stream;
    for (const t of stream.getVideoTracks()) {
      if ('contentHint' in t) t.contentHint = 'motion';
      t.addEventListener('ended', () => void this.stop());
    }
    this.#events = openEvents(
      `${this.#base}/rooms/${this.#room}/events`,
      this.#token,
      (event, data) => this.#onEvent(event, data as Record<string, unknown>),
      (state) => {
        if (state === 'gone') void this.stop();
      },
    );
    return this.#session();
  }

  /** New PIN and join-link secret; disconnects every viewer. */
  async newPin(): Promise<CastSession> {
    if (!this.active) throw new Error('not casting');
    const r = await api<{ pin: string; key: string }>(`${this.#base}/rooms/${this.#room}/pin`, {
      method: 'POST',
      token: this.#token,
    });
    this.#pin = r.pin;
    this.#key = r.key;
    return this.#session();
  }

  /** Disconnects one viewer. */
  async kick(viewerId: string): Promise<void> {
    if (!this.active) return;
    await api(`${this.#base}/rooms/${this.#room}/kick`, { method: 'POST', token: this.#token, body: { viewer: viewerId } });
  }

  async stop(): Promise<void> {
    if (!this.active && !this.#stream) return;
    const token = this.#token;
    const room = this.#room;
    this.#token = '';
    this.#events?.close();
    this.#events = null;
    for (const id of [...this.#peers.keys()]) this.#hangup(id);
    if (this.#ownsStream) this.#stream?.getTracks().forEach((t) => t.stop());
    this.#stream = null;
    if (token) {
      try {
        await api(`${this.#base}/rooms/${room}`, { method: 'DELETE', token });
      } catch {
        /* room may already be gone */
      }
    }
    this.emit('ended', undefined);
  }

  // ---------------------------------------------------------------------------------------

  #session(): CastSession {
    const room = this.#room;
    const key = this.#key;
    return {
      room,
      pin: this.#pin,
      joinUrl: (viewerPageUrl: string) => {
        const u = new URL(viewerPageUrl, location.href);
        u.hash = `room=${room}&key=${key}`; // the fragment is never sent to any server
        return u.toString();
      },
    };
  }

  async #capture(source: CastSource, audio: boolean): Promise<MediaStream> {
    this.#ownsStream = false;
    if (source instanceof MediaStream) return source;
    if (typeof HTMLCanvasElement !== 'undefined' && source instanceof HTMLCanvasElement) {
      this.#ownsStream = true;
      return source.captureStream(this.#opts.maxFramerate);
    }
    if (typeof HTMLVideoElement !== 'undefined' && source instanceof HTMLVideoElement) {
      const v = source as HTMLVideoElement & { captureStream?: () => MediaStream; mozCaptureStream?: () => MediaStream };
      const s = v.captureStream?.() ?? v.mozCaptureStream?.();
      if (!s) throw new Error('This browser cannot capture a <video> element. Use source: "screen" instead.');
      return s;
    }
    if (!navigator.mediaDevices?.getDisplayMedia) {
      throw new Error('Screen capture is not available in this browser (it needs a desktop browser and HTTPS).');
    }
    this.#ownsStream = true;
    return navigator.mediaDevices.getDisplayMedia({
      video: { frameRate: { ideal: this.#opts.maxFramerate, max: this.#opts.maxFramerate } },
      audio: audio ? { echoCancellation: false, noiseSuppression: false, autoGainControl: false } : false,
    } as DisplayMediaStreamOptions);
  }

  #onEvent(event: string, d: Record<string, unknown>) {
    switch (event) {
      case 'ready':
        for (const id of (d.viewers as string[]) ?? []) void this.#call(id);
        break;
      case 'viewer-joined':
        if (typeof d.id === 'string') void this.#call(d.id);
        break;
      case 'viewer-left':
        if (typeof d.id === 'string') this.#hangup(d.id);
        this.emit('viewers', this.viewerCount);
        break;
      case 'pin':
        if (typeof d.pin === 'string' && typeof d.key === 'string') {
          this.#pin = d.pin;
          this.#key = d.key;
          this.emit('pin', { pin: d.pin, room: this.#room });
        }
        break;
      case 'signal':
        if (typeof d.from === 'string') void this.#onSignal(d.from, d.data as Signal);
        break;
    }
  }

  async #send(to: string, data: Signal) {
    if (!this.#token) return;
    try {
      await api(`${this.#base}/rooms/${this.#room}/signal`, { method: 'POST', token: this.#token, body: { to, data } });
    } catch {
      /* viewer went away */
    }
  }

  async #call(id: string) {
    if (!this.#stream) return;
    this.#hangup(id, false); // the new offer replaces the old connection; no "bye" race
    const pc = new RTCPeerConnection({ iceServers: this.#opts.iceServers });
    const peer: Peer = { pc, pending: [] };
    this.#peers.set(id, peer);
    for (const track of this.#stream.getTracks()) {
      const init: RTCRtpTransceiverInit = { direction: 'sendonly', streams: [this.#stream] };
      if (track.kind === 'video') {
        init.sendEncodings = [{ maxBitrate: this.#opts.maxBitrate, maxFramerate: this.#opts.maxFramerate }];
      }
      pc.addTransceiver(track, init);
    }
    pc.onicecandidate = (e) => {
      if (e.candidate && e.candidate.candidate) void this.#send(id, { type: 'candidate', candidate: iceCandidateInit(e.candidate) });
    };
    pc.onconnectionstatechange = () => {
      if (this.#peers.get(id)?.pc !== pc) return;
      this.emit('viewers', this.viewerCount);
      if (pc.connectionState === 'failed') {
        setTimeout(() => {
          if (this.#peers.get(id)?.pc === pc && this.#stream) void this.#call(id);
        }, 2000);
      }
    };
    await pc.setLocalDescription(await pc.createOffer());
    await this.#send(id, { type: 'offer', sdp: pc.localDescription!.sdp });
  }

  async #onSignal(from: string, data: Signal) {
    const peer = this.#peers.get(from);
    if (!peer) return;
    try {
      if (data.type === 'answer') {
        await peer.pc.setRemoteDescription({ type: 'answer', sdp: data.sdp });
        for (const c of peer.pending.splice(0)) await peer.pc.addIceCandidate(c);
      } else if (data.type === 'candidate') {
        if (peer.pc.remoteDescription) await peer.pc.addIceCandidate(data.candidate);
        else peer.pending.push(data.candidate);
      }
    } catch (e) {
      this.emit('error', e instanceof Error ? e : new Error(String(e)));
    }
  }

  #hangup(id: string, sayBye = true) {
    const peer = this.#peers.get(id);
    if (!peer) return;
    this.#peers.delete(id);
    if (sayBye && this.#token) void this.#send(id, { type: 'bye' });
    peer.pc.onconnectionstatechange = null;
    peer.pc.close();
  }
}
