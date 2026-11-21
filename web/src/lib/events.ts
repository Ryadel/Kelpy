import { openEvents, readEvents, Unauthorized, Unreachable, type RuntimeEvent } from './api';
import { loadSession } from './session';

/** Kelpy sends a keep-alive every 15 seconds: a longer silence is a dropped connection. */
const SILENCE = 45_000;
/** How soon a dropped subscription is opened again. */
const RETRY = 2_000;
/**
 * How long a hidden page stays subscribed. Browsers open only a few connections to one server, shared by all its
 * tabs, and a page nobody sees needs none: it catches up when shown again.
 */
const HIDDEN = 30_000;

export interface EventHandlers {
  onEvent: (event: RuntimeEvent) => void;
  /** Subscribed again after a break, in which events may have been missed: read again what the page shows. */
  onResumed: () => void;
  /** The page's session ended: the subscription is over. */
  onEnded: (reason: unknown) => void;
}

export interface Watch {
  /** Settles once the first subscription is open, or fails with it; loading after it misses no change. */
  subscribed: Promise<void>;
  stop: () => void;
}

type SharedEvent =
  | { type: 'hello'; tab: string }
  | { type: 'subscribed'; to?: string }
  | { type: 'event'; event: RuntimeEvent }
  | { type: 'ended' };

/**
 * One authenticated event stream for tabs sharing a session. With HTTP/1.1, a stream per tab can occupy every
 * browser connection to Kelpy, leaving even the requests that finish loading a tab queued behind the streams.
 */
export function watchEvents(handlers: EventHandlers): Watch {
  const session = loadSession();
  if (!session || !navigator.locks || typeof BroadcastChannel === 'undefined') {
    return watchStream(handlers, true);
  }

  const name = `kelpy.events.${session.id}`;
  const tab = crypto.randomUUID();
  const channel = new BroadcastChannel(name);
  const lifetime = new AbortController();
  let leader: Watch | undefined;
  let ready = false;
  let first = true;
  let resolveReady!: () => void;
  let rejectReady!: (reason: unknown) => void;
  const subscribed = new Promise<void>((resolve, reject) => {
    resolveReady = resolve;
    rejectReady = reject;
  });
  let finish!: () => void;
  const done = new Promise<void>((resolve) => { finish = resolve; });
  const send = (message: SharedEvent) => channel.postMessage(message);
  const stillSignedIn = () => loadSession()?.id === session.id;

  function available() {
    if (first) {
      first = false;
      resolveReady();
    } else {
      handlers.onResumed();
    }
  }

  function stop() {
    if (lifetime.signal.aborted) return;
    lifetime.abort();
    leader?.stop();
    finish();
    channel.close();
    window.removeEventListener('storage', sessionChanged);
  }

  function ended(reason: unknown) {
    if (first) rejectReady(reason);
    else handlers.onEnded(reason);
    stop();
  }

  function sessionChanged() {
    if (!stillSignedIn()) ended(new Unauthorized());
  }

  channel.onmessage = ({ data }: MessageEvent<SharedEvent>) => {
    if (!stillSignedIn()) {
      sessionChanged();
      return;
    }
    switch (data.type) {
      case 'hello':
        if (ready) send({ type: 'subscribed', to: data.tab });
        break;
      case 'subscribed':
        if (data.to === undefined || data.to === tab) available();
        break;
      case 'event':
        handlers.onEvent(data.event);
        break;
      case 'ended':
        ended(new Unauthorized());
        break;
    }
  };
  window.addEventListener('storage', sessionChanged);
  send({ type: 'hello', tab });

  void navigator.locks.request(name, { signal: lifetime.signal }, async () => {
    if (lifetime.signal.aborted) return;
    function announce() {
      if (!stillSignedIn()) return sessionChanged();
      ready = true;
      send({ type: 'subscribed' });
      available();
    }
    function endStream(reason: unknown) {
      if (reason instanceof Unauthorized) send({ type: 'ended' });
      ended(reason);
    }
    leader = watchStream({
      onEvent(event) {
        if (!stillSignedIn()) return sessionChanged();
        send({ type: 'event', event });
        handlers.onEvent(event);
      },
      onResumed: announce,
      onEnded: endStream,
    }, false);
    // Race the first subscription with stop so closing a tab also releases a lock acquired while offline.
    try {
      await Promise.race([leader.subscribed, done]);
      if (!lifetime.signal.aborted) announce();
      await done;
    } catch (reason: unknown) {
      endStream(reason);
    }
  }).catch((reason: unknown) => {
    if (!lifetime.signal.aborted) ended(reason);
  });

  return { subscribed, stop };
}

/**
 * Follows Kelpy's events while the page is shown, opening the subscription again whenever it drops. It ends with
 * `stopping`, when the session ends, or with `stop`.
 */
function watchStream({ onEvent, onResumed, onEnded }: EventHandlers, pauseWhenHidden: boolean): Watch {
  let first = true;
  let stopped = false;
  /** Closed for a hidden page, until it is shown again. */
  let paused = false;
  let controller: AbortController | null = null;
  let retry: ReturnType<typeof setTimeout> | undefined;
  let hide: ReturnType<typeof setTimeout> | undefined;
  let settle!: (reason?: unknown) => void;
  const subscribed = new Promise<void>((resolve, reject) => {
    settle = (reason) => (reason === undefined ? resolve() : reject(reason));
  });

  async function open() {
    const mine = new AbortController();
    controller = mine;
    let last: RuntimeEvent | null = null;
    try {
      const response = await openEvents(mine.signal);
      if (first) {
        first = false;
        settle();
      } else {
        onResumed();
      }
      await readEvents<RuntimeEvent>(
        response,
        (event) => {
          last = event;
          onEvent(event);
        },
        SILENCE,
      );
    } catch (reason: unknown) {
      if (mine.signal.aborted) {
        return;
      }
      if (first && !(reason instanceof Unreachable)) {
        stop();
        settle(reason);
        return;
      }
      if (reason instanceof Unauthorized) {
        stop();
        onEnded(reason);
        return;
      }
    }
    if ((last as RuntimeEvent | null)?.type === 'stopping') {
      stop();
    } else if (!stopped && !mine.signal.aborted) {
      // Dropped, or Kelpy cannot be reached for now: the page already says so meanwhile.
      retry = setTimeout(() => void open(), RETRY);
    }
  }

  function pause() {
    paused = true;
    clearTimeout(retry);
    controller?.abort();
  }

  function onVisibility() {
    clearTimeout(hide);
    if (document.visibilityState === 'hidden') {
      hide = setTimeout(() => !first && !stopped && pause(), HIDDEN);
    } else if (paused && !stopped) {
      paused = false;
      void open();
    }
  }

  function stop() {
    stopped = true;
    clearTimeout(retry);
    clearTimeout(hide);
    controller?.abort();
    document.removeEventListener('visibilitychange', onVisibility);
  }

  if (pauseWhenHidden) document.addEventListener('visibilitychange', onVisibility);
  void open();
  return { subscribed, stop };
}
