import { openEvents, readEvents, Unauthorized, type RuntimeEvent } from './api';

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

/**
 * Follows Kelpy's events while the page is shown, opening the subscription again whenever it drops. It ends with
 * `stopping`, when the session ends, or with `stop`.
 */
export function watchEvents({ onEvent, onResumed, onEnded }: EventHandlers): Watch {
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
      if (first) {
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

  document.addEventListener('visibilitychange', onVisibility);
  void open();
  return { subscribed, stop };
}
