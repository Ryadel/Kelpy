import {
  askElevation,
  cancelElevation,
  chooseMemory,
  describe,
  fetchElevation,
  fetchMemory,
  fetchStatus,
  type ElevationView,
  type MemoryUse,
  type MemoryView,
} from './api';
import { runtime } from './runtime.svelte';

/** Set before the page reloads onto the Kelpy restarted as administrator, so that it says so once reloaded. */
const RESTARTED = 'kelpy.restarted';
/** How long the page waits for the new Kelpy once the old one hands over. */
const COMING_BACK = 45_000;

/**
 * Restarting Kelpy, as the whole page sees it: whether and how it can take administrator rights, the memory of the
 * server it uses, and the restart this page asked for, shown in a dialog until the page reloads onto the new Kelpy.
 */
export const elevation = $state<{
  view: ElevationView | null;
  /** The memory of the server Kelpy uses, and the one chosen. */
  memory: MemoryView | null;
  /** The restart this page asked for: shown until the page reloads, or closed. */
  asked: boolean;
  /** What it is for: administrator rights, or the memory to use. */
  reason: { kind: 'administrator' } | { kind: 'memory'; use: MemoryUse };
  /** Why it did not happen, once it did not. */
  failure: string | null;
  /** The new Kelpy did not answer in time. */
  lost: boolean;
  /** The failed actions whose offer was put aside with *Not now*. */
  dismissed: number[];
}>({
  view: null,
  memory: null,
  asked: false,
  reason: { kind: 'administrator' },
  failure: null,
  lost: false,
  dismissed: [],
});

/** Whether this page can ask for the restart: Kelpy is not elevated, and the system can ask for consent. */
export const canRestart = () =>
  elevation.view !== null && !elevation.view.elevated && (elevation.view.how === 'prompt' || elevation.view.how === 'terminal');

export async function refreshElevation() {
  try {
    [elevation.view, elevation.memory] = await Promise.all([fetchElevation(), fetchMemory()]);
  } catch {
    // Unreachable for now: the page says so meanwhile, and reads it again when Kelpy is back.
  }
}

/** Asks Kelpy to restart as administrator, then follows it until the page reloads onto the new Kelpy. */
export async function restartAsAdministrator() {
  elevation.reason = { kind: 'administrator' };
  await follow(async () => {
    await askElevation();
    return true;
  });
}

/**
 * Asks Kelpy to restart with the memory `use`, as administrator for the shared one when it is not, then follows it.
 * When Kelpy uses that memory already, as the private one while the shared one chosen cannot be opened, it is only
 * recorded as the one chosen, without a restart; nothing happens when it is both already.
 */
export async function switchMemory(use: MemoryUse) {
  const memory = elevation.memory;
  if (memory?.use === use && memory.chosen === use) {
    return;
  }
  elevation.reason = { kind: 'memory', use };
  if (memory?.use === use) {
    // Only recorded: no restart to follow, and a dialog only if it fails.
    try {
      const chosen = await chooseMemory(use);
      if (chosen.memory) {
        elevation.memory = chosen.memory;
      }
    } catch (reason: unknown) {
      elevation.lost = false;
      elevation.failure = describe(reason);
      elevation.asked = true;
    }
    return;
  }
  await follow(async () => (await chooseMemory(use)).restarts);
}

/** Whether a choice of the memory `use` only records it, without a restart: Kelpy uses it already. */
export const recordsOnly = (use: MemoryUse) => elevation.memory?.use === use;

/** Whether choosing the memory `use` restarts Kelpy through the system's prompt: the shared one, not as administrator. */
export const prompts = (use: MemoryUse) => use === 'shared' && !(elevation.view?.elevated ?? false);

/**
 * Asks for a restart with `ask`, which says whether one begins, then follows it: the system's prompt if any, then the
 * new Kelpy, onto which the page reloads as soon as it answers, with the same session.
 */
async function follow(ask: () => Promise<boolean>) {
  const before = runtime.status?.pid;
  const elevated = elevation.view?.elevated ?? false;
  elevation.asked = true;
  elevation.failure = null;
  elevation.lost = false;
  try {
    if (!(await ask())) {
      elevation.asked = false;
      return;
    }
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
        // Told once reloaded, if it took administrator rights.
        if (!elevated) {
          sessionStorage.setItem(RESTARTED, '1');
        }
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
