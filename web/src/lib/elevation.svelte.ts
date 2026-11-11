import { askElevation, cancelElevation, describe, fetchElevation, fetchStatus, type ElevationView } from './api';
import { runtime } from './runtime.svelte';

/** Set before the page reloads onto the Kelpy restarted as administrator, so that it says so once reloaded. */
const RESTARTED = 'kelpy.restarted';
/** How long the page waits for the new Kelpy once the old one hands over. */
const COMING_BACK = 45_000;

/**
 * Restarting Kelpy as administrator, as the whole page sees it: whether and how it can, and the restart this page
 * asked for, shown in a dialog until the page reloads onto the new Kelpy.
 */
export const elevation = $state<{
  view: ElevationView | null;
  /** The restart this page asked for: shown until the page reloads, or closed. */
  asked: boolean;
  /** Why it did not happen, once it did not. */
  failure: string | null;
  /** The new Kelpy did not answer in time. */
  lost: boolean;
  /** The failed actions whose offer was put aside with *Not now*. */
  dismissed: number[];
}>({ view: null, asked: false, failure: null, lost: false, dismissed: [] });

/** Whether this page can ask for the restart: Kelpy is not elevated, and the system can ask for consent. */
export const canRestart = () =>
  elevation.view !== null && !elevation.view.elevated && (elevation.view.how === 'prompt' || elevation.view.how === 'terminal');

export async function refreshElevation() {
  try {
    elevation.view = await fetchElevation();
  } catch {
    // Unreachable for now: the page says so meanwhile, and reads it again when Kelpy is back.
  }
}

/**
 * Asks Kelpy to restart as administrator, then follows it: the system's prompt, then the new Kelpy, onto which the
 * page reloads as soon as it answers, with the same session.
 */
export async function restartAsAdministrator() {
  const before = runtime.status?.pid;
  elevation.asked = true;
  elevation.failure = null;
  elevation.lost = false;
  try {
    await askElevation();
  } catch (reason: unknown) {
    elevation.failure = describe(reason);
    return;
  }
  let handingOver: number | null = null;
  while (elevation.asked) {
    await new Promise((resolve) => setTimeout(resolve, 1000));
    try {
      const status = await fetchStatus();
      if (before !== undefined && status.pid !== before) {
        sessionStorage.setItem(RESTARTED, '1');
        location.reload();
        return;
      }
      const view = await fetchElevation();
      elevation.view = view;
      if (view.step === 'failed') {
        elevation.failure = view.reason;
        return;
      }
      if (view.step === 'restarting') {
        handingOver ??= Date.now();
      }
    } catch {
      // The old Kelpy stopped and the new one is starting.
      handingOver ??= Date.now();
    }
    if (handingOver !== null && Date.now() - handingOver > COMING_BACK) {
      elevation.lost = true;
      return;
    }
  }
}

/** Cancels the restart while the system's prompt is open: Kelpy goes on as it was. */
export async function cancelRestart() {
  try {
    await cancelElevation();
  } catch {
    // Too late: the restart goes on, and the page reloads with it.
    return;
  }
  elevation.asked = false;
}

/** Whether this page was just reloaded onto the Kelpy restarted as administrator: told once. */
export function justRestarted(): boolean {
  const restarted = sessionStorage.getItem(RESTARTED) !== null;
  sessionStorage.removeItem(RESTARTED);
  return restarted;
}
