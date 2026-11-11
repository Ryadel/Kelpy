import { expect, test } from 'bun:test';
import { QUOTES, randomQuote } from '../src/lib/quotes';

test('a quote is one of the list, whatever the random number', () => {
  expect(randomQuote(() => 0)).toBe(QUOTES[0]);
  expect(randomQuote(() => 0.999_999)).toBe(QUOTES[QUOTES.length - 1]);
  expect(QUOTES).toContain(randomQuote());
});
