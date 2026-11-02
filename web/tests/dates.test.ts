import { describe, expect, test } from 'bun:test';
import { groupByDate } from '../src/lib/dates';

/** Unix time, in seconds, of a local date and time. */
const at = (day: number, hour = 12) => new Date(2026, 9, day, hour).getTime() / 1000;
const names = (times: number[], now: Date) =>
  groupByDate(times.map((updated_at) => ({ updated_at })), now).map((group) => [group.name, group.items.length]);

describe('conversations grouped by date', () => {
  const now = new Date(2026, 9, 31, 9);

  test('follow the local calendar day, not the last 24 hours', () => {
    expect(names([at(31, 0), at(30, 23), at(30, 0), at(29, 23)], now)).toEqual([
      ['Today', 1],
      ['Yesterday', 2],
      ['Previous 7 days', 1],
    ]);
  });

  test('reach back seven days before Older begins', () => {
    expect(names([at(24, 0), at(23, 23)], now)).toEqual([
      ['Previous 7 days', 1],
      ['Older', 1],
    ]);
  });

  test('leave out empty groups', () => {
    expect(groupByDate([], now)).toEqual([]);
    expect(names([at(31), at(1)], now)).toEqual([
      ['Today', 1],
      ['Older', 1],
    ]);
  });
});
