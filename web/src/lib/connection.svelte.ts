import { identify, loadSession } from './session';

/**
 * Whether the Kelpy runtime can be reached. Every request reports it, and a light check repeats while the page is
 * open, so a stopped Kelpy or a dropped SSH tunnel shows at once, not at the user's next action.
 */

/** How often the check runs while Kelpy answers. The check is not activity: it never keeps an idle Kelpy running. */
const CHECK_EVERY = 5_000;
/** How often it runs while Kelpy cannot be reached, so the page reconnects soon after the connection returns. */
const RETRY_EVERY = 2_000;

export const connection = $state({
  reachable: true,
  /**
   * This page's session ended by itself: Kelpy started again, or the browser was signed out elsewhere. The page never
   * asks for the access code then: a page that reconnected by itself may be talking to another program that took
   * the port, and the code typed there would let it into Kelpy. The user opens the link of the terminal instead.
   */
  expired: false,
  /** Counts the returns after Kelpy could not be reached, so that what the page shows catches up. */
  resumed: 0,
});

/** Whether the page can send to Kelpy now. */
export const usable = () => connection.reachable && !connection.expired;

/** Starts checking the connection; returns the function that stops it. */
export function watchConnection(): () => void {
  let timer: ReturnType<typeof setTimeout> | undefined;
  let checking = false;
  let stopped = false;

  async function check() {
    if (checking || stopped) {
      return;
    }
    checking = true;
    clearTimeout(timer);
    const session = loadSession();
    if (session !== null) {
      // With a session, only the Kelpy that issued it counts as reachable: another program may have taken the port.
      // A Kelpy without it does count: the next request says that the session ended.
      const identity = await identify(session, 0);
      connection.reachable = identity === 'kelpy' || identity === 'ended';
    } else {
      try {
        await fetch('/api/health', { cache: 'no-store' });
        connection.reachable = true;
      } catch {
        connection.reachable = false;
      }
    }
    checking = false;
    if (!stopped) {
      timer = setTimeout(check, connection.reachable ? CHECK_EVERY : RETRY_EVERY);
    }
  }

  // Browsers slow down timers in hidden tabs: coming back, or back online, checks at once.
  const checkNow = () => {
    if (document.visibilityState === 'visible') {
      void check();
    }
  };
  document.addEventListener('visibilitychange', checkNow);
  window.addEventListener('online', checkNow);
  void check();
  return () => {
    stopped = true;
    clearTimeout(timer);
    document.removeEventListener('visibilitychange', checkNow);
    window.removeEventListener('online', checkNow);
  };
}
