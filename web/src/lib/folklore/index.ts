export interface FolkloreEntry {
  readonly id: string;
  readonly subject: string;
  readonly tradition: string;
  readonly text: string;
  readonly source: string;
  readonly author: string;
  readonly license: string;
  readonly licenseUrl: string;
}

/** A shuffled deck: every subject appears once before the next cycle starts. */
export class FolkloreDeck {
  #remaining: FolkloreEntry[] = [];
  #previous: string | undefined;

  constructor(private readonly entries: readonly FolkloreEntry[]) {
    if (entries.length === 0) {
      throw new Error('The folklore catalog is empty');
    }
  }

  next(): FolkloreEntry {
    if (this.#remaining.length === 0) {
      this.#remaining = [...this.entries];
      for (let i = this.#remaining.length - 1; i > 0; i--) {
        const j = Math.floor(Math.random() * (i + 1));
        [this.#remaining[i], this.#remaining[j]] = [this.#remaining[j], this.#remaining[i]];
      }
      const last = this.#remaining.length - 1;
      if (last > 0 && this.#remaining[last].id === this.#previous) {
        [this.#remaining[0], this.#remaining[last]] = [this.#remaining[last], this.#remaining[0]];
      }
    }
    const entry = this.#remaining.pop()!;
    this.#previous = entry.id;
    return entry;
  }
}

let deck: Promise<FolkloreDeck> | undefined;

/** Bundled locally and loaded only when the first answer needs a waiting hint. */
export function nextFolkloreEntry(): Promise<FolkloreEntry> {
  deck ??= Promise.all([import('./world.json'), import('./scp.json')])
    .then(([world, scp]) => new FolkloreDeck([
      ...world.default.map((entry) => ({
        ...entry,
        author: 'Wikipedia contributors',
        license: 'CC BY-SA 4.0',
        licenseUrl: 'https://creativecommons.org/licenses/by-sa/4.0/',
      })),
      ...scp.default.map((entry) => ({
        ...entry,
        license: 'CC BY-SA 3.0',
        licenseUrl: 'https://creativecommons.org/licenses/by-sa/3.0/',
      })),
    ]))
    .catch((error: unknown) => {
      deck = undefined;
      throw error;
    });
  return deck.then((catalog) => catalog.next());
}
