/** Browser helpers shared by CastHost and CastViewer. */

type Listener<T> = (value: T) => void;

/** Minimal typed event emitter. */
export class Emitter<Events extends Record<string, unknown>> {
  #listeners = new Map<keyof Events, Set<Listener<never>>>();

  /** Subscribe; returns an unsubscribe function. */
  on<K extends keyof Events>(event: K, fn: Listener<Events[K]>): () => void {
    let set = this.#listeners.get(event);
    if (!set) this.#listeners.set(event, (set = new Set()));
    set.add(fn as Listener<never>);
    return () => set!.delete(fn as Listener<never>);
  }

  protected emit<K extends keyof Events>(event: K, value: Events[K]): void {
    for (const fn of this.#listeners.get(event) ?? []) {
      try {
        (fn as Listener<Events[K]>)(value);
      } catch (e) {
        console.error(e);
      }
    }
  }
}

export class CastError extends Error {
  constructor(message: string, readonly status: number) {
    super(message);
    this.name = 'CastError';
  }
}

export function apiBase(server: string): string {
  return server.replace(/\/+$/, '');
}

export async function api<T>(
  url: string,
  init: { method?: string; token?: string; body?: unknown } = {},
): Promise<T> {
  const headers: Record<string, string> = {};
  if (init.token) headers.Authorization = `Bearer ${init.token}`;
  if (init.body !== undefined) headers['Content-Type'] = 'application/json';
  const res = await fetch(url, {
    method: init.method ?? 'GET',
    headers,
    body: init.body === undefined ? undefined : JSON.stringify(init.body),
    credentials: 'same-origin',
    cache: 'no-store',
  });
  if (res.status === 204) return undefined as T;
  const data = (await res.json().catch(() => ({}))) as Record<string, unknown>;
  if (!res.ok) throw new CastError(typeof data.error === 'string' ? data.error : `HTTP ${res.status}`, res.status);
  return data as T;
}

export interface EventStream {
  close(): void;
}

/**
 * Server-sent events over fetch, so the token travels in the Authorization header (never in
 * the URL, where it would end up in logs and history). Reconnects with backoff; stops for
 * good on 401/404 and calls `onGone`.
 */
export function openEvents(
  url: string,
  token: string,
  onEvent: (event: string, data: unknown) => void,
  onState: (state: 'open' | 'reconnecting' | 'gone') => void,
): EventStream {
  let closed = false;
  let controller: AbortController | null = null;
  let delay = 500;

  const run = async () => {
    while (!closed) {
      controller = new AbortController();
      try {
        const res = await fetch(url, {
          headers: { Authorization: `Bearer ${token}`, Accept: 'text/event-stream' },
          cache: 'no-store',
          credentials: 'same-origin',
          signal: controller.signal,
        });
        if (res.status === 401 || res.status === 404) {
          onState('gone');
          return;
        }
        if (!res.ok || !res.body) throw new Error(`HTTP ${res.status}`);
        onState('open');
        delay = 500;
        const reader = res.body.pipeThrough(new TextDecoderStream()).getReader();
        let buf = '';
        for (;;) {
          const { value, done } = await reader.read();
          if (done) break;
          buf += value;
          let sep: number;
          while ((sep = buf.indexOf('\n\n')) >= 0) {
            const block = buf.slice(0, sep);
            buf = buf.slice(sep + 2);
            let event = 'message';
            let data = '';
            for (const line of block.split('\n')) {
              if (line.startsWith('event:')) event = line.slice(6).trim();
              else if (line.startsWith('data:')) data += line.slice(5).trim();
            }
            if (!data) continue;
            try {
              onEvent(event, JSON.parse(data));
            } catch {
              /* ignore malformed event */
            }
          }
        }
      } catch {
        /* network error or abort */
      }
      if (closed) return;
      onState('reconnecting');
      await new Promise((r) => setTimeout(r, delay));
      delay = Math.min(delay * 2, 8000);
    }
  };
  run();
  return {
    close() {
      closed = true;
      controller?.abort();
    },
  };
}

/** Plain-object copy of an ICE candidate for the wire. */
export function iceCandidateInit(c: RTCIceCandidate) {
  return { candidate: c.candidate, sdpMid: c.sdpMid, sdpMLineIndex: c.sdpMLineIndex };
}
