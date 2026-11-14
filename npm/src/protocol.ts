/**
 * Wire protocol shared by the browser clients and the Node signaling server.
 * Only these four message types ever travel between host and viewers; everything is
 * re-built from validated fields (nothing is relayed as-is).
 */

export interface IceCandidateInit {
  candidate: string;
  sdpMid: string | null;
  sdpMLineIndex: number | null;
}

export type Signal =
  | { type: 'offer'; sdp: string }
  | { type: 'answer'; sdp: string }
  | { type: 'candidate'; candidate: IceCandidateInit }
  | { type: 'bye' };

export const LIMITS = {
  sdp: 32 * 1024,
  candidate: 2048,
  mid: 64,
  body: 64 * 1024,
} as const;

/** Returns a clean copy of a signal, or `null` if it is malformed or too large. */
export function sanitizeSignal(input: unknown): Signal | null {
  if (!input || typeof input !== 'object') return null;
  const s = input as Record<string, unknown>;
  switch (s.type) {
    case 'offer':
    case 'answer': {
      const sdp = s.sdp;
      if (typeof sdp !== 'string' || !sdp.startsWith('v=0') || sdp.length > LIMITS.sdp) return null;
      return { type: s.type, sdp };
    }
    case 'candidate': {
      const c = s.candidate as Record<string, unknown> | null | undefined;
      if (!c || typeof c !== 'object') return null;
      const candidate = c.candidate;
      const mid = c.sdpMid ?? null;
      const index = c.sdpMLineIndex ?? null;
      if (typeof candidate !== 'string' || candidate.length > LIMITS.candidate) return null;
      if (mid !== null && (typeof mid !== 'string' || mid.length > LIMITS.mid)) return null;
      if (index !== null && (typeof index !== 'number' || !Number.isInteger(index) || index < 0 || index > 64)) {
        return null;
      }
      return { type: 'candidate', candidate: { candidate, sdpMid: mid, sdpMLineIndex: index } };
    }
    case 'bye':
      return { type: 'bye' };
    default:
      return null;
  }
}

/** Room codes: 6 characters without look-alikes (no 0/O, 1/I/L). */
export const ROOM_ALPHABET = 'ABCDEFGHJKMNPQRSTUVWXYZ23456789';
export const ROOM_RE = /^[ABCDEFGHJKMNPQRSTUVWXYZ23456789]{6}$/;
export const PIN_RE = /^\d{6}$/;
export const KEY_RE = /^[0-9a-f]{32}$/;
