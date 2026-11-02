/**
 * The Web UI's session with Kelpy, sent as `Authorization`. It is kept in the storage of this origin, which includes
 * the port, and never in a cookie: browsers send a host's cookies to all its ports, so any other local service could
 * replay it. The tabs of this origin share it; it ends on sign-out or when Kelpy stops.
 *
 * Before the session is sent, whoever answers on the port proves it is the Kelpy that issued it: an SSH tunnel may
 * drop and another program take its local port. The proof is an HMAC of a random challenge, keyed by the session, so
 * the session never travels for it (see `identify`).
 */

const KEY = 'kelpy.session';
/** What the proof signs before the challenge; the same in Kelpy (`auth.rs`). */
const PROOF_CONTEXT = 'kelpy-session-proof\n';
/** How long a proof covers the requests that follow it, in milliseconds. */
const PROOF_COVERS = 1_000;

/** A session as Kelpy issued it: `session` is the credential, `id` names it in identity proofs. */
export interface StoredSession {
  id: string;
  session: string;
}

export function loadSession(): StoredSession | null {
  const text = localStorage.getItem(KEY);
  if (text === null) {
    return null;
  }
  try {
    const { id, session } = JSON.parse(text) as Partial<StoredSession>;
    return typeof id === 'string' && typeof session === 'string' ? { id, session } : null;
  } catch {
    // Kept by an earlier version: signing in again replaces it.
    return null;
  }
}

export function saveSession(stored: StoredSession) {
  localStorage.setItem(KEY, JSON.stringify(stored));
}

/** Forgets `stored` if it is still the one kept: another tab may have signed in since. */
export function forgetSession(stored: StoredSession | null) {
  if (stored !== null && loadSession()?.session === stored.session) {
    localStorage.removeItem(KEY);
  }
}

/**
 * Who answers on Kelpy's port: the Kelpy that issued the session (`kelpy`), a Kelpy without it, which ended it or
 * started again (`ended`), another program (`other`), or nobody (`unreachable`).
 */
export type Identity = 'kelpy' | 'ended' | 'other' | 'unreachable';

/** The last proof that succeeded, and when. */
let proven: { session: string; at: number } | null = null;
/** The proof under way: the requests sent meanwhile wait for it instead of asking again. */
let asking: { session: string; answer: Promise<Identity> } | null = null;

/** Forgets the last proof: the connection dropped, so the next request asks again. */
export function distrust() {
  proven = null;
}

/** Asks who answers, unless a proof for `stored` succeeded within the last `covers` milliseconds. */
export function identify(stored: StoredSession, covers = PROOF_COVERS): Promise<Identity> {
  if (proven?.session === stored.session && performance.now() - proven.at < covers) {
    return Promise.resolve('kelpy');
  }
  if (asking?.session !== stored.session) {
    const answer = prove(stored).finally(() => {
      if (asking?.answer === answer) {
        asking = null;
      }
    });
    asking = { session: stored.session, answer };
  }
  return asking.answer;
}

async function prove(stored: StoredSession): Promise<Identity> {
  const challenge = hex(crypto.getRandomValues(new Uint8Array(32)));
  let response: Response;
  try {
    response = await fetch('/api/session/proof', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ id: stored.id, challenge }),
      cache: 'no-store',
    });
  } catch {
    distrust();
    return 'unreachable';
  }
  if (response.status === 404) {
    distrust();
    return 'ended';
  }
  const body = (await response.json().catch(() => null)) as { proof?: unknown } | null;
  const expected = await sign(stored.session, PROOF_CONTEXT + challenge);
  if (!response.ok || body?.proof !== expected) {
    distrust();
    return 'other';
  }
  proven = { session: stored.session, at: performance.now() };
  return 'kelpy';
}

/** HMAC-SHA256 of `message` keyed by `key`, in hexadecimal. */
export async function sign(key: string, message: string): Promise<string> {
  const encoder = new TextEncoder();
  const secret = await crypto.subtle.importKey('raw', encoder.encode(key), { name: 'HMAC', hash: 'SHA-256' }, false, [
    'sign',
  ]);
  return hex(new Uint8Array(await crypto.subtle.sign('HMAC', secret, encoder.encode(message))));
}

function hex(bytes: Uint8Array): string {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('');
}
