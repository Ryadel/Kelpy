import type { Action, ActionState, Message } from './api';

/** A piece of an answer as the conversation shows it: text, or the actions taken at that point. */
export type Segment = { kind: 'text'; text: string } | { kind: 'actions'; actions: Action[] };

const encoder = new TextEncoder();
const decoder = new TextDecoder();

/** An answer's text with its actions where the model asked for them. Their positions count UTF-8 bytes. */
export function segments(message: Message): Segment[] {
  if (message.actions.length === 0) {
    return message.content ? [{ kind: 'text', text: message.content }] : [];
  }
  const bytes = encoder.encode(message.content);
  const pieces: Segment[] = [];
  let from = 0;
  const text = (to: number) => {
    const piece = decoder.decode(bytes.subarray(from, to)).trim();
    if (piece) {
      pieces.push({ kind: 'text', text: piece });
    }
    from = to;
  };
  for (const action of message.actions) {
    text(Math.min(Math.max(action.position, from), bytes.length));
    const last = pieces.at(-1);
    if (last?.kind === 'actions') {
      last.actions.push(action);
    } else {
      pieces.push({ kind: 'actions', actions: [action] });
    }
  }
  text(bytes.length);
  return pieces;
}

/** Whether the action is under way: waiting for approval or running. */
export const pending = (action: Action) => action.state === 'waiting' || action.state === 'running';

export const stateLabels: Record<ActionState, string> = {
  waiting: 'Waiting for approval',
  running: 'Running',
  done: 'Done',
  failed: 'Failed',
  denied: 'Denied',
};

/** A duration in milliseconds, as `340 ms`, `2.4 s`, `3 min 5 s` or `1 h 2 min`. */
export function duration(milliseconds: number): string {
  if (milliseconds < 1000) {
    return `${milliseconds} ms`;
  }
  if (milliseconds < 60_000) {
    return `${(milliseconds / 1000).toFixed(1)} s`;
  }
  return elapsed(Math.floor(milliseconds / 1000));
}

/** A time under way, in whole seconds, as `12 s`, `3 min 5 s` or `1 h 2 min`. */
export function elapsed(seconds: number): string {
  const whole = Math.max(0, seconds);
  if (whole < 60) {
    return `${whole} s`;
  }
  if (whole < 3600) {
    return `${Math.floor(whole / 60)} min ${whole % 60} s`;
  }
  return `${Math.floor(whole / 3600)} h ${Math.floor((whole % 3600) / 60)} min`;
}

/** A Unix time, in seconds, as the time of day: `14:05:09`. */
export function clock(seconds: number): string {
  return new Date(seconds * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
}
