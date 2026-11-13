import { describe, expect, test } from 'bun:test';
import { Thumbnails } from '../src/lib/thumbnails';

/** Thumbnails keeping `kept` unused, with the object URLs made and freed, and the files fetched. */
function thumbnails(kept: number) {
  const made: string[] = [];
  const freed: string[] = [];
  const fetched: string[] = [];
  const cache = new Thumbnails(
    async (name) => {
      fetched.push(name);
      if (name === 'gone.png') {
        throw new Error('not found');
      }
      return new Blob([name]);
    },
    (image) => {
      const url = `blob:${made.length}:${image.type}`;
      made.push(url);
      return url;
    },
    (url) => freed.push(url),
    kept,
  );
  return { cache, made, freed, fetched };
}

/** Lets the promises settle. */
const settled = () => new Promise((resolve) => setTimeout(resolve, 0));

describe('Thumbnails', () => {
  test('an image shown in several places is fetched once and freed only once none shows it, after the others', async () => {
    const { cache, made, freed, fetched } = thumbnails(1);
    expect(await cache.acquire('a.png')).toBe('blob:0:image/png');
    expect(await cache.acquire('a.png')).toBe('blob:0:image/png');
    expect(fetched).toEqual(['a.png']);
    cache.release('a.png');
    await cache.acquire('b.jpg');
    cache.release('b.jpg');
    await settled();
    // a.png is still shown; b.jpg is the one unused kept.
    expect(freed).toEqual([]);
    cache.release('a.png');
    await settled();
    expect(freed).toEqual(['blob:1:image/jpeg']);
    expect(cache.size).toBe(1);
    // Shown again while kept: not fetched again.
    expect(await cache.acquire('a.png')).toBe(made[0]);
    expect(fetched).toEqual(['a.png', 'b.jpg']);
  });

  test('an image just uploaded shows from its file, and is freed like the others', async () => {
    const { cache, freed, fetched } = thumbnails(0);
    cache.seed('shot.png', new Blob(['pixels']));
    // Seeded and unused, beyond what is kept: freed at once, as when removed from a message never sent.
    await settled();
    expect(freed).toEqual(['blob:0:image/png']);
    const { cache: kept, fetched: none } = thumbnails(4);
    kept.seed('shot.png', new Blob(['pixels']));
    expect(await kept.acquire('shot.png')).toBe('blob:0:image/png');
    expect(none).toEqual([]);
    expect(fetched).toEqual([]);
  });

  test('an image taken off a message is freed as soon as nothing shows it', async () => {
    const { cache, freed } = thumbnails(8);
    cache.seed('shot.png', new Blob(['pixels']));
    await cache.acquire('shot.png');
    cache.forget('shot.png');
    await settled();
    expect(freed).toEqual([]);
    cache.release('shot.png');
    await settled();
    expect(freed).toEqual(['blob:0:image/png']);
    expect(cache.size).toBe(0);
  });

  test('an image no longer in Files shows as missing', async () => {
    const { cache, made } = thumbnails(4);
    expect(await cache.acquire('gone.png')).toBeNull();
    cache.release('gone.png');
    cache.release('gone.png');
    expect(made).toEqual([]);
  });
});
