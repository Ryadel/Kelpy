import type { Action } from './api';

/**
 * What an action's error says when the account lacks administrator rights, as Windows, Linux and the tools on them
 * word it: then Kelpy offers to restart as administrator under the action.
 */
const NEEDS_ADMINISTRATOR =
  /access (is )?denied|only administrators|permission denied|operation not permitted|requires elevation|run as administrator|administrator (rights|privileges)|must be (run as )?root|are you root|interactive authentication required|not in the sudoers/i;

/** The end of a command's output that says why it failed: the rest is what it did before. */
const OUTPUT_TAIL = 4000;

/** Whether `error` says the action needed administrator rights. */
export function needsAdministrator(error: string | null | undefined): boolean {
  return error != null && NEEDS_ADMINISTRATOR.test(error);
}

/**
 * Whether `action` failed for lack of administrator rights: its error says so, or it is a command that ran and
 * ended with an exit code other than 0, its output saying so.
 */
export function failedForRights(action: Pick<Action, 'state' | 'error' | 'exit_code' | 'output'>): boolean {
  switch (action.state) {
    case 'failed':
      return needsAdministrator(action.error);
    case 'done':
      return action.exit_code !== null && action.exit_code !== 0 && needsAdministrator(action.output.slice(-OUTPUT_TAIL));
    default:
      return false;
  }
}
