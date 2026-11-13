<script lang="ts" module>
  import { fetchFile } from './api';
  import { Thumbnails } from './thumbnails';

  /** The page's thumbnails, held by the thumbnails shown. */
  const thumbnails = new Thumbnails(
    fetchFile,
    (image) => URL.createObjectURL(image),
    (url) => URL.revokeObjectURL(url),
  );

  /** Shows the image `name`, just uploaded, from `file`, instead of fetching it again. */
  export function seedThumbnail(name: string, file: Blob) {
    thumbnails.seed(name, file);
  }

  /** The image `name` will not show again here: its object URL is freed once nothing shows it. */
  export function forgetThumbnail(name: string) {
    thumbnails.forget(name);
  }
</script>

<script lang="ts">
  import type { Snippet } from 'svelte';

  interface Props {
    /** The uploaded image. */
    name: string;
    /** Shows it, from its object URL. */
    children: Snippet<[string]>;
    /** Shown instead when it can no longer be fetched, such as once deleted. */
    missing?: Snippet;
  }

  let { name, children, missing }: Props = $props();

  /** Undefined while it is fetched. */
  let url = $state<string | null | undefined>(undefined);

  // Held while shown: its object URL is freed once nothing shows it for a while.
  $effect(() => {
    const held = name;
    let current = true;
    url = undefined;
    void thumbnails.acquire(held).then((found) => {
      if (current) {
        url = found;
      }
    });
    return () => {
      current = false;
      thumbnails.release(held);
    };
  });
</script>

{#if url}
  {@render children(url)}
{:else if url === null}
  {@render missing?.()}
{/if}
