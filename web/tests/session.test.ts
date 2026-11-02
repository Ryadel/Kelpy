import { afterEach, beforeEach, describe, expect, test } from 'bun:test';
import { distrust, forgetSession, identify, loadSession, saveSession, sign } from '../src/lib/session';

/** The browser's storage, shared like the tabs of one origin share it. */
const storage = new Map<string, string>();
globalThis.localStorage = {
  getItem: (key: string) => storage.get(key) ?? null,
  setItem: (key: string, value: string) => void storage.set(key, value),
  removeItem: (key: string) => void storage.delete(key),
} as Storage;

const old = { id: 'id-old', session: 'old' };
const fresh = { id: 'id-new', session: 'new' };

describe('the Web UI session', () => {
  beforeEach(() => storage.clear());

  test('is forgotten when it ends', () => {
    saveSession(old);
    forgetSession(old);
    expect(loadSession()).toBeNull();
  });

  test('signed in again by another tab is kept when the old one ends', () => {
    saveSession(old);
    saveSession(fresh);
    forgetSession(old);
    expect(loadSession()).toEqual(fresh);
  });

  test('kept by an earlier version, as the bare session, counts as none', () => {
    storage.set('kelpy.session', '4325e4261');
    expect(loadSession()).toBeNull();
  });
});

describe('the identity proof', () => {
  const realFetch = globalThis.fetch;
  /** The requests the page sent, with their Authorization header. */
  let sent: { path: string; authorization: string | null }[] = [];

  /** Answers the proof as `answer` decides, from the challenge received. */
  function serve(answer: (challenge: string) => Promise<Response>) {
    globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
      sent.push({ path: String(input), authorization: new Headers(init?.headers).get('Authorization') });
      const { challenge } = JSON.parse(String(init?.body)) as { challenge: string };
      return answer(challenge);
    }) as typeof fetch;
  }
  const kelpy = (session: string) => (challenge: string) =>
    sign(session, `kelpy-session-proof\n${challenge}`).then((proof) => Response.json({ proof }));

  beforeEach(() => {
    sent = [];
    distrust();
  });
  afterEach(() => {
    globalThis.fetch = realFetch;
  });

  test('matches Kelpy', async () => {
    // The same vector is in crates/kelpy-gateway/src/auth.rs.
    expect(await sign('kelpy-test-session', 'kelpy-session-proof\nchallenge')).toBe(
      '6079b53b72a45385ddc747fdf258937625f9027ec19c78439d1f57ca05846375',
    );
  });

  test('recognises the Kelpy that issued the session, without sending it', async () => {
    serve(kelpy('new'));
    expect(await identify(fresh)).toBe('kelpy');
    expect(sent).toEqual([{ path: '/api/session/proof', authorization: null }]);
  });

  test('recognises another program, whatever it answers', async () => {
    serve(kelpy('a session it made up'));
    expect(await identify(fresh)).toBe('other');
    serve(async () => Response.json({ status: 'ok' }));
    expect(await identify(fresh)).toBe('other');
    serve(async () => new Response('<html></html>'));
    expect(await identify(fresh)).toBe('other');
  });

  test('says when Kelpy no longer has the session, or nobody answers', async () => {
    serve(async () => Response.json({ error: 'no such session' }, { status: 404 }));
    expect(await identify(fresh)).toBe('ended');
    serve(() => Promise.reject(new TypeError('Failed to fetch')));
    expect(await identify(fresh)).toBe('unreachable');
  });

  test('covers the requests that follow it for a moment, and asks again after a drop', async () => {
    serve(kelpy('new'));
    await Promise.all([identify(fresh), identify(fresh), identify(fresh)]);
    expect(sent).toHaveLength(1);
    await identify(fresh);
    expect(sent).toHaveLength(1);
    expect(await identify(fresh, 0)).toBe('kelpy');
    expect(sent).toHaveLength(2);
    distrust();
    await identify(fresh);
    expect(sent).toHaveLength(3);
  });
});
