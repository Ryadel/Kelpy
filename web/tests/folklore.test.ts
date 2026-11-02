import { describe, expect, test } from 'bun:test';
import { FolkloreDeck, type FolkloreEntry } from '../src/lib/folklore';
import world from '../src/lib/folklore/world.json';
import scp from '../src/lib/folklore/scp.json';

const entries = [...world, ...scp];
const normalize = (text: string) => text.normalize('NFKD').toLowerCase().replace(/[^\p{L}\p{N}]/gu, '');

describe('folklore catalog', () => {
  test('contains at least 1,000 distinct subjects and texts, with no alias padding', () => {
    expect(entries.length).toBeGreaterThanOrEqual(1000);
    for (const field of ['id', 'subject', 'source', 'text'] as const) {
      expect(new Set(entries.map((entry) => normalize(entry[field]))).size).toBe(entries.length);
    }
  });

  test('every entry is short, self-contained, sourced and safe to render as plain text', () => {
    for (const entry of entries) {
      expect(entry.text.length).toBeLessThanOrEqual(235);
      expect(entry.text.trim().split(/\s+/).length).toBeLessThanOrEqual(40);
      expect(entry.text).not.toMatch(/[\r\n]|\[\[|\]\]|\uFFFD|<script|█/);
      expect(entry.tradition.trim()).not.toBe('');
      expect(entry.tradition).not.toMatch(/\[\[|\]\]/);
      expect(new URL(entry.source).protocol).toBe('https:');
    }
    for (const entry of scp) expect(entry.author.trim()).not.toBe('');
    for (const entry of world) expect(entry.revision).toBeGreaterThan(0);
  });

  test('includes the requested legends and Internet folklore, excluding movie-only inventions', () => {
    const names = new Set(entries.map((entry) => entry.subject));
    for (const name of ['Kelpie', 'Baba Yaga', 'Bogeyman', 'Chupacabra', 'Slender Man', 'The Backrooms', 'SCP-173', 'SCP-999']) {
      expect(names.has(name)).toBe(true);
    }
    for (const name of ['Babadook', 'The Babadook', 'Dracula', "Frankenstein's monster", 'Cthulhu', 'Godzilla', 'King Kong']) {
      expect(names.has(name)).toBe(false);
    }
  });
});

describe('waiting-hint selection', () => {
  const catalog: FolkloreEntry[] = entries.map((entry) => ({
    ...entry, author: 'test', license: 'test', licenseUrl: 'https://example.com/license',
  }));

  test('exhausts the catalog before repeating and avoids repeats across cycle boundaries', () => {
    const deck = new FolkloreDeck(catalog);
    let previous: string | undefined;
    for (let cycle = 0; cycle < 5; cycle++) {
      const seen = new Set<string>();
      for (let index = 0; index < catalog.length; index++) {
        const entry = deck.next();
        expect(entry.id).not.toBe(previous);
        expect(seen.has(entry.id)).toBe(false);
        seen.add(entry.id);
        previous = entry.id;
      }
      expect(seen.size).toBe(catalog.length);
    }
  });

  test('handles a single entry and rejects an empty catalog', () => {
    expect(() => new FolkloreDeck([])).toThrow('empty');
    const deck = new FolkloreDeck([catalog[0]]);
    expect(deck.next()).toBe(catalog[0]);
    expect(deck.next()).toBe(catalog[0]);
  });
});
