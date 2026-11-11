/** Short sayings of the trade, one at random on the first run's welcome; the install scripts print the same. */
export const QUOTES = [
  'There is no place like 127.0.0.1.',
  'It is always DNS. Except when it is the firewall.',
  'Read the logs. Then read them again.',
  'A backup you have never restored is a hope, not a backup.',
  'Measure twice, restart once.',
  'Change one thing at a time.',
  'Every server is a pet until it is 3 a.m.',
  'Nothing is more permanent than a temporary fix.',
] as const;

export function randomQuote(random: () => number = Math.random): string {
  return QUOTES[Math.floor(random() * QUOTES.length)];
}
