import { describe, expect, test } from 'bun:test';
import type { Session } from '../src/lib/api';
import { kept, merged } from '../src/lib/conversations';

function session(id: number, revision: number, access: Session['access'], updated_at = 100): Session {
  return { id, title: `Conversation ${id}`, explanations: false, access, approval: 'ask', created_at: 1, updated_at, revision };
}

describe('conversations', () => {
  test('a copy that arrives late does not replace a newer one', () => {
    // Set to None, then Full: the event for None arrives last.
    const full = session(1, 6, 'full');
    const none = session(1, 5, 'none');
    expect(kept([full], none)).toEqual([full]);
    expect(kept([none], full)).toEqual([full]);
  });

  test('a list fetched while events arrive keeps the newer copies, most recent first', () => {
    const listed = [session(1, 6, 'full', 100)];
    const fetched = [session(2, 0, 'read_only', 50), session(1, 5, 'none', 90)];
    const list = merged(listed, fetched);
    expect(list.map((one) => [one.id, one.access])).toEqual([
      [1, 'full'],
      [2, 'read_only'],
    ]);
    // A conversation the fetched list no longer has was deleted.
    expect(merged([session(3, 1, 'none')], [])).toEqual([]);
  });
});
