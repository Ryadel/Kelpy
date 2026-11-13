/** An image's object URL, and how many places show it. */
interface Entry {
  url: Promise<string | null>;
  users: number;
  /** Freed once no place shows it, not kept. */
  forgotten?: boolean;
}

/**
 * The thumbnails of uploaded images, by name, as object URLs. Each place showing one holds it, from `acquire` to
 * `release`: one no place shows is kept a while, as it often shows again (a message once sent, a conversation opened
 * again), up to `kept` of them; then the oldest is freed.
 */
export class Thumbnails {
  private readonly entries = new Map<string, Entry>();
  /** The names of the entries no place shows, oldest first. */
  private readonly unused = new Set<string>();

  constructor(
    private readonly fetch: (name: string) => Promise<Blob>,
    private readonly url: (image: Blob) => string,
    private readonly revoke: (url: string) => void,
    private readonly kept = 8,
  ) {}

  /** Shows `name` from `file`, the image just uploaded, instead of fetching it again. */
  seed(name: string, file: Blob) {
    const entry = this.entries.get(name);
    if (entry && entry.users > 0) {
      return;
    }
    if (entry) {
      this.free(name, entry);
    }
    this.entries.set(name, { url: Promise.resolve(this.url(typed(name, file))), users: 0 });
    this.park(name);
  }

  /** The object URL of the image `name`, or null if it can no longer be fetched, such as once deleted. */
  acquire(name: string): Promise<string | null> {
    let entry = this.entries.get(name);
    if (!entry) {
      entry = {
        url: this.fetch(name)
          .then((blob) => this.url(typed(name, blob)))
          .catch(() => null),
        users: 0,
      };
      this.entries.set(name, entry);
    }
    entry.users += 1;
    entry.forgotten = false;
    this.unused.delete(name);
    return entry.url;
  }

  /** One place no longer shows `name`. */
  release(name: string) {
    const entry = this.entries.get(name);
    if (!entry || entry.users === 0) {
      return;
    }
    entry.users -= 1;
    if (entry.users === 0 && entry.forgotten) {
      this.free(name, entry);
    } else if (entry.users === 0) {
      this.park(name);
    }
  }

  /** `name` will not show again, such as an image taken off a message: freed once no place shows it. */
  forget(name: string) {
    const entry = this.entries.get(name);
    if (entry && entry.users === 0) {
      this.free(name, entry);
    } else if (entry) {
      entry.forgotten = true;
    }
  }

  /** How many object URLs are kept: for tests. */
  get size(): number {
    return this.entries.size;
  }

  private park(name: string) {
    this.unused.delete(name);
    this.unused.add(name);
    for (const oldest of this.unused) {
      if (this.unused.size <= this.kept) {
        break;
      }
      const entry = this.entries.get(oldest);
      if (entry) {
        this.free(oldest, entry);
      }
    }
  }

  private free(name: string, entry: Entry) {
    this.entries.delete(name);
    this.unused.delete(name);
    void entry.url.then((url) => url && this.revoke(url));
  }
}

/** `image` with the type of `name`'s extension: Kelpy serves every file as bytes. */
function typed(name: string, image: Blob): Blob {
  const extension = name.split('.').pop()?.toLowerCase();
  const type = extension === 'jpg' || extension === 'jpeg' ? 'image/jpeg' : `image/${extension ?? 'png'}`;
  return new Blob([image], { type });
}
