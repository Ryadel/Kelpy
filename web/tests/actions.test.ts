import { describe, expect, test } from 'bun:test';
import {
  commandLog,
  diffLines,
  duration,
  elapsed,
  mergeOperations,
  restorable,
  segments,
  stepStates,
} from '../src/lib/actions';
import type { Action, Message } from '../src/lib/api';

const action = (id: number, position: number): Action => ({
  id,
  session_id: 1,
  message_id: 2,
  step: id,
  position,
  tool: 'disk_usage',
  reason: '',
  summary: 'Show the disk usage',
  command: 'df -h',
  state: 'done',
  exit_code: 0,
  output: '',
  error: null,
  created_at: 0,
  duration_ms: 12,
  started_at: 0,
  change: null,
  phase: null,
  plan: null,
  plan_step: null,
});

const answer = (content: string, actions: Action[]): Message => ({
  id: 2,
  session_id: 1,
  role: 'assistant',
  content,
  status: 'complete',
  model: null,
  created_at: 0,
  actions,
});

describe('an answer with actions', () => {
  test('shows each action where the model asked for it, counting UTF-8 bytes', () => {
    // "Verifico è" takes 11 bytes: the accented letter takes two.
    const content = 'Verifico è\n\nIl disco è pieno al 40%.';
    const pieces = segments(answer(content, [action(1, 11), action(2, 11)]));
    expect(pieces.map((piece) => piece.kind)).toEqual(['text', 'actions', 'text']);
    expect(pieces[0]).toEqual({ kind: 'text', text: 'Verifico è' });
    expect(pieces[1].kind === 'actions' && pieces[1].actions.map((shown) => shown.id)).toEqual([1, 2]);
    expect(pieces[2]).toEqual({ kind: 'text', text: 'Il disco è pieno al 40%.' });
  });

  test('may start with its actions and have no text yet', () => {
    const pieces = segments(answer('', [action(1, 0)]));
    expect(pieces.map((piece) => piece.kind)).toEqual(['actions']);
    expect(segments(answer('', []))).toEqual([]);
  });

  test('durations read as milliseconds, seconds, minutes or hours', () => {
    expect(duration(340)).toBe('340 ms');
    expect(duration(2400)).toBe('2.4 s');
    expect(duration(185_400)).toBe('3 min 5 s');
    expect(duration(3_725_000)).toBe('1 h 2 min');
    expect([elapsed(-1), elapsed(12), elapsed(60)]).toEqual(['0 s', '12 s', '1 min 0 s']);
  });
});

describe('a plan and the Command log', () => {
  const step = { tool: 'install_packages', summary: 'Install nginx', detail: null, floor: null };
  const plan: Action = {
    ...action(5, 0),
    tool: 'propose_plan',
    plan: { goal: 'Serve the site', steps: [step, step, step] },
  };
  const carrying = (id: number, index: number, state: Action['state']): Action => ({
    ...action(id, 0),
    state,
    plan_step: { plan_id: 5, step: index },
  });

  test('each step shows the last action that carried it out', () => {
    const actions = [plan, carrying(6, 0, 'failed'), carrying(7, 0, 'done'), carrying(8, 1, 'uncertain')];
    expect(stepStates(plan, actions)).toEqual(['done', 'uncertain', 'pending']);
    // Another plan's steps are not this one's.
    expect(stepStates(plan, [{ ...carrying(9, 2, 'done'), plan_step: { plan_id: 4, step: 2 } }])).toEqual([
      'pending',
      'pending',
      'pending',
    ]);
  });

  test("lists the answers' actions and the administrator's in the order asked for", () => {
    const restore: Action = { ...action(4, 0), message_id: null, tool: 'restore_file' };
    expect(commandLog([answer('', [action(3, 0), action(6, 0)])], [restore]).map((shown) => shown.id)).toEqual([
      3, 4, 6,
    ]);
  });

  test('an operation learned of while the conversation was read keeps its latest state', () => {
    const running: Action = { ...action(4, 0), message_id: null, state: 'running' };
    const ended: Action = { ...running, state: 'done' };
    const started: Action = { ...action(7, 0), message_id: null, state: 'running' };
    expect(mergeOperations([running], [ended, started]).map((shown) => [shown.id, shown.state])).toEqual([
      [4, 'done'],
      [7, 'running'],
    ]);
  });

  test('a change can be restored once it ran and was backed up', () => {
    const change = { diff: '--- a\n+++ a\n@@ -1 +1 @@\n-x\n+y\n', notes: [], floor: null };
    expect(restorable({ ...action(1, 0), change })).toBe(false);
    expect(restorable({ ...action(1, 0), change: { ...change, backup: '/b/1-a' } })).toBe(true);
    expect(restorable({ ...action(1, 0), state: 'failed', change: { ...change, backup: '/b/1-a' } })).toBe(false);
    expect(diffLines(change.diff)).toEqual(['@@ -1 +1 @@', '-x', '+y']);
  });
});
