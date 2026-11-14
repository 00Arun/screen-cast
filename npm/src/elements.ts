/**
 * Drop-in web components.
 *
 *   import 'secure-screen-cast/elements';
 *
 *   <!-- page that casts -->
 *   <screen-cast-host server="/cast" viewer-url="/watch.html"></screen-cast-host>
 *   <screen-cast-host server="/cast" source="#player"></screen-cast-host>   <!-- a <video>/<canvas> -->
 *
 *   <!-- page that watches -->
 *   <screen-cast-viewer server="/cast"></screen-cast-viewer>
 *
 * Style them with CSS custom properties (--sc-accent, --sc-bg, --sc-fg, --sc-radius) and ::part().
 */
import { CastHost, type CastSession, type CastSource } from './host.js';
import { CastViewer, type ViewerStatus } from './viewer.js';

const BASE_CSS = `
:host { --sc-accent: #2563eb; --sc-bg: #111827; --sc-fg: #f9fafb; --sc-muted: #9ca3af; --sc-radius: 12px;
  display: block; font: 14px/1.4 system-ui, -apple-system, "Segoe UI", Roboto, sans-serif; color: var(--sc-fg); }
* { box-sizing: border-box; }
button { font: inherit; border: 0; border-radius: 8px; padding: 9px 14px; cursor: pointer; background: var(--sc-accent); color: #fff; }
button.secondary { background: rgba(255,255,255,.12); }
button:disabled { opacity: .6; cursor: default; }
input { font: inherit; padding: 9px 10px; border-radius: 8px; border: 1px solid rgba(255,255,255,.18); background: rgba(0,0,0,.25); color: inherit; min-width: 0; }
.card { background: var(--sc-bg); border-radius: var(--sc-radius); padding: 14px; }
.muted { color: var(--sc-muted); }
.row { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }
.err { color: #fca5a5; min-height: 1.2em; }
[hidden] { display: none !important; }
`;

function el<K extends keyof HTMLElementTagNameMap>(tag: K, props: Record<string, unknown> = {}, ...kids: (Node | string)[]) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (k === 'part') e.setAttribute('part', String(v));
    else (e as unknown as Record<string, unknown>)[k] = v;
  }
  e.append(...kids);
  return e;
}

function parseIce(attr: string | null): RTCIceServer[] | undefined {
  if (!attr) return undefined;
  try {
    const v = JSON.parse(attr);
    return Array.isArray(v) ? v : undefined;
  } catch {
    return undefined;
  }
}

// ---------------------------------------------------------------------------------------------

/** `<screen-cast-host server viewer-url source audio ice-servers>` */
export class ScreenCastHostElement extends HTMLElement {
  static observedAttributes = ['server'];
  host: CastHost | null = null;
  #session: CastSession | null = null;
  #ui!: {
    start: HTMLButtonElement;
    panel: HTMLDivElement;
    room: HTMLElement;
    pin: HTMLElement;
    viewers: HTMLElement;
    link: HTMLInputElement;
    copy: HTMLButtonElement;
    newPin: HTMLButtonElement;
    stop: HTMLButtonElement;
    err: HTMLElement;
  };

  connectedCallback() {
    if (this.shadowRoot) return;
    const root = this.attachShadow({ mode: 'open' });
    const style = el('style');
    style.textContent = BASE_CSS + `.code { font-weight: 700; letter-spacing: .12em; font-size: 18px; }`;
    const ui = {
      start: el('button', { part: 'start', textContent: 'Start casting' }),
      panel: el('div', { hidden: true }),
      room: el('span', { className: 'code' }),
      pin: el('span', { className: 'code' }),
      viewers: el('span', { className: 'muted' }),
      link: el('input', { readOnly: true, ariaLabel: 'Viewer link' }),
      copy: el('button', { className: 'secondary', textContent: 'Copy link' }),
      newPin: el('button', { className: 'secondary', textContent: 'New PIN' }),
      stop: el('button', { textContent: 'Stop' }),
      err: el('div', { className: 'err', role: 'status' }),
    };
    ui.stop.style.background = '#dc2626';
    ui.link.style.flex = '1';
    ui.panel.append(
      el('div', { className: 'row' }, el('span', { className: 'muted', textContent: 'Room' }), ui.room,
        el('span', { className: 'muted', textContent: 'PIN' }), ui.pin, ui.viewers),
      el('div', { className: 'row' }, ui.link, ui.copy),
      el('div', { className: 'row' }, ui.newPin, ui.stop),
    );
    ui.panel.style.display = 'grid';
    ui.panel.style.gap = '10px';
    root.append(style, el('div', { className: 'card', part: 'card' }, ui.start, ui.panel, ui.err));
    this.#ui = ui;

    ui.start.addEventListener('click', () => void this.start());
    ui.stop.addEventListener('click', () => void this.stop());
    ui.newPin.addEventListener('click', async () => {
      try {
        this.#show(await this.host!.newPin());
      } catch (e) {
        this.#error(e);
      }
    });
    ui.copy.addEventListener('click', async () => {
      try {
        await navigator.clipboard.writeText(ui.link.value);
        ui.copy.textContent = 'Copied';
        setTimeout(() => (ui.copy.textContent = 'Copy link'), 1500);
      } catch {
        ui.link.select();
      }
    });
  }

  disconnectedCallback() {
    void this.host?.stop();
  }

  #source(): CastSource {
    const s = this.getAttribute('source');
    if (!s || s === 'screen') return 'screen';
    const target = document.querySelector(s);
    if (target instanceof HTMLVideoElement || target instanceof HTMLCanvasElement) return target;
    throw new Error(`source "${s}" is not a <video> or <canvas> element`);
  }

  async start(): Promise<void> {
    const ui = this.#ui;
    ui.err.textContent = '';
    ui.start.disabled = true;
    try {
      this.host = new CastHost({
        server: this.getAttribute('server') || '/cast',
        iceServers: parseIce(this.getAttribute('ice-servers')),
      });
      this.host.on('viewers', (n) => {
        ui.viewers.textContent = n === 1 ? '· 1 viewer' : `· ${n} viewers`;
        this.dispatchEvent(new CustomEvent('cast-viewers', { detail: { viewers: n } }));
      });
      this.host.on('pin', () => this.#session && void this.host && this.#refresh());
      this.host.on('ended', () => this.#reset());
      const session = await this.host.start({ source: this.#source(), audio: this.getAttribute('audio') !== 'false' });
      this.#show(session);
      ui.start.hidden = true;
      ui.panel.hidden = false;
    } catch (e) {
      this.#error(e);
      this.#reset();
    } finally {
      ui.start.disabled = false;
    }
  }

  async stop(): Promise<void> {
    await this.host?.stop();
  }

  #refresh() {
    // The server rotated the PIN (too many wrong guesses): ask for the current values.
    this.host?.newPin().then((s) => this.#show(s), (e) => this.#error(e));
  }

  #show(session: CastSession) {
    this.#session = session;
    const viewerUrl = this.getAttribute('viewer-url') || location.href;
    const joinUrl = session.joinUrl(viewerUrl);
    this.#ui.room.textContent = session.room;
    this.#ui.pin.textContent = session.pin;
    this.#ui.link.value = joinUrl;
    this.#ui.viewers.textContent = '· 0 viewers';
    this.dispatchEvent(new CustomEvent('cast-start', { detail: { room: session.room, pin: session.pin, joinUrl } }));
  }

  #reset() {
    if (!this.#ui) return;
    this.#session = null;
    this.#ui.start.hidden = false;
    this.#ui.panel.hidden = true;
    this.dispatchEvent(new CustomEvent('cast-stop'));
  }

  #error(e: unknown) {
    this.#ui.err.textContent = e instanceof Error ? e.message : String(e);
  }
}

// ---------------------------------------------------------------------------------------------

const STATUS_TEXT: Record<ViewerStatus, string> = {
  idle: '',
  joining: 'Joining…',
  waiting: 'Waiting for the cast to start…',
  connecting: 'Connecting…',
  live: 'Live',
  reconnecting: 'Reconnecting…',
  ended: 'The cast has ended.',
  kicked: 'You were disconnected by the host.',
};

/** `<screen-cast-viewer server ice-servers>` */
export class ScreenCastViewerElement extends HTMLElement {
  viewer: CastViewer | null = null;

  connectedCallback() {
    if (this.shadowRoot) return;
    const root = this.attachShadow({ mode: 'open' });
    const style = el('style');
    style.textContent =
      BASE_CSS +
      `.stage { position: relative; background: #000; border-radius: var(--sc-radius); overflow: hidden; aspect-ratio: 16 / 9; }
       video { width: 100%; height: 100%; object-fit: contain; display: block; background: #000; }
       .bar { position: absolute; left: 0; right: 0; bottom: 0; display: flex; gap: 8px; align-items: center;
         justify-content: space-between; padding: 10px 12px; background: linear-gradient(transparent, rgba(0,0,0,.75)); }
       .sound { position: absolute; left: 50%; top: 50%; transform: translate(-50%, -50%); font-size: 17px;
         padding: 12px 20px; border-radius: 999px; box-shadow: 0 8px 30px rgba(0,0,0,.5); }
       form { display: flex; gap: 8px; flex-wrap: wrap; } form input { flex: 1 1 110px; text-align: center; letter-spacing: .15em; }`;

    const video = el('video', { playsInline: true, autoplay: true, muted: true, part: 'video' });
    video.disablePictureInPicture = true;
    const status = el('span', { className: 'muted' });
    const unmute = el('button', { className: 'secondary', textContent: 'Sound on' });
    const fs = el('button', { className: 'secondary', textContent: 'Fullscreen' });
    const sound = el('button', { className: 'sound', textContent: 'Tap to turn on sound', hidden: true });
    const stage = el('div', { className: 'stage', part: 'stage', hidden: true }, video, sound,
      el('div', { className: 'bar' }, status, el('div', { className: 'row' }, unmute, fs)));

    const room = el('input', { placeholder: 'Room code', maxLength: 6, autocomplete: 'off', ariaLabel: 'Room code' });
    const pin = el('input', { placeholder: 'PIN', maxLength: 6, inputMode: 'numeric', autocomplete: 'off', ariaLabel: 'PIN' });
    const go = el('button', { type: 'submit', textContent: 'Watch' });
    const err = el('div', { className: 'err', role: 'status' });
    const form = el('form', {}, room, pin, go);
    const card = el('div', { className: 'card', part: 'card' },
      el('div', { className: 'muted', textContent: 'Enter the room code and PIN shown on the casting screen.' }), form, err);
    card.style.display = 'grid';
    card.style.gap = '10px';
    root.append(style, card, stage);

    const viewer = new CastViewer({
      server: this.getAttribute('server') || '/cast',
      video,
      iceServers: parseIce(this.getAttribute('ice-servers')),
    });
    this.viewer = viewer;

    const updateMute = () => (unmute.textContent = video.muted ? 'Sound on' : 'Mute');
    viewer.on('status', (s) => {
      status.textContent = STATUS_TEXT[s];
      const watching = s !== 'idle' && s !== 'joining';
      stage.hidden = !watching;
      card.hidden = watching && s !== 'ended' && s !== 'kicked';
      if (s === 'ended' || s === 'kicked') err.textContent = STATUS_TEXT[s];
      this.dispatchEvent(new CustomEvent('cast-status', { detail: { status: s } }));
    });
    viewer.on('stream', async () => {
      // Try with sound; browsers may require a tap first.
      video.muted = false;
      try {
        await video.play();
        sound.hidden = true;
      } catch {
        video.muted = true;
        video.play().catch(() => undefined);
        sound.hidden = false;
      }
      updateMute();
    });
    sound.addEventListener('click', () => {
      video.muted = false;
      video.play().catch(() => undefined);
      sound.hidden = true;
      updateMute();
    });
    unmute.addEventListener('click', () => {
      video.muted = !video.muted;
      sound.hidden = true;
      updateMute();
    });
    fs.addEventListener('click', () => {
      const v = video as HTMLVideoElement & { webkitEnterFullscreen?: () => void };
      if (document.fullscreenElement) void document.exitFullscreen();
      else if (stage.requestFullscreen) void stage.requestFullscreen().catch(() => undefined);
      else v.webkitEnterFullscreen?.();
    });
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      err.textContent = '';
      go.disabled = true;
      try {
        await viewer.join({ room: room.value, pin: pin.value });
      } catch (x) {
        err.textContent = x instanceof Error ? x.message : String(x);
      } finally {
        go.disabled = false;
      }
    });
    viewer.joinFromUrl().catch((x) => (err.textContent = x instanceof Error ? x.message : String(x)));
  }

  disconnectedCallback() {
    this.viewer?.leave();
  }
}

if (typeof customElements !== 'undefined') {
  if (!customElements.get('screen-cast-host')) customElements.define('screen-cast-host', ScreenCastHostElement);
  if (!customElements.get('screen-cast-viewer')) customElements.define('screen-cast-viewer', ScreenCastViewerElement);
}

declare global {
  interface HTMLElementTagNameMap {
    'screen-cast-host': ScreenCastHostElement;
    'screen-cast-viewer': ScreenCastViewerElement;
  }
}
