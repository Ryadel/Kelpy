/** A run of conversations listed under one heading, such as "Today". */
export interface DateGroup<T> {
  name: string;
  items: T[];
}

/**
 * Splits items, most recently active first, into Today, Yesterday, Previous 7 days and Older, by the local calendar
 * day of `updated_at` (Unix time, in seconds). Empty groups are left out.
 */
export function groupByDate<T extends { updated_at: number }>(items: T[], now: Date): DateGroup<T>[] {
  // Day boundaries come from the calendar, not from 24-hour steps, so a daylight saving change cannot shift them.
  const dayStart = (daysAgo: number) => new Date(now.getFullYear(), now.getMonth(), now.getDate() - daysAgo).getTime();
  const bounds: [string, number][] = [
    ['Today', dayStart(0)],
    ['Yesterday', dayStart(1)],
    ['Previous 7 days', dayStart(7)],
    ['Older', -Infinity],
  ];
  const groups: DateGroup<T>[] = [];
  for (const item of items) {
    const name = bounds.find(([, start]) => item.updated_at * 1000 >= start)![0];
    if (groups.at(-1)?.name === name) {
      groups.at(-1)!.items.push(item);
    } else {
      groups.push({ name, items: [item] });
    }
  }
  return groups;
}
