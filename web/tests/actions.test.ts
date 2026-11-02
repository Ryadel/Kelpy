import { describe, expect, test } from 'bun:test';
import { duration, elapsed, segments } from '../src/lib/actions';
import type { Action, Message } from '../src/lib/api';

const action = (id: number, position: number): Action => ({
  id,
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
