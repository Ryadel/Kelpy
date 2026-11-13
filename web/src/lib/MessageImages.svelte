<script lang="ts">
  import ImageIcon from '@lucide/svelte/icons/image';
  import type { MessageImage } from './images';
  import Thumbnail from './Thumbnail.svelte';

  interface Props {
    images: MessageImage[];
  }

  let { images }: Props = $props();
</script>

<!-- The images of a user's message: each one's thumbnail, opened whole in a new tab, and the text read from it. -->
<div class="flex flex-col items-end gap-2">
  <div class="flex flex-wrap justify-end gap-2">
    {#each images as image (image.name)}
      <Thumbnail name={image.name}>
        {#snippet children(url)}
          <a href={url} target="_blank" rel="noopener" title="{image.name}: open it whole">
            <img src={url} alt={image.name} class="max-h-48 max-w-64 rounded-2xl border object-contain" />
          </a>
        {/snippet}
        {#snippet missing()}
          <span
            class="text-muted-foreground inline-flex items-center gap-1.5 rounded-2xl border px-3 py-2 text-xs"
            title="No longer in Files"
          >
            <ImageIcon class="size-4" />
            {image.name}
          </span>
        {/snippet}
      </Thumbnail>
    {/each}
  </div>
  {#each images as image (image.name)}
    {#if image.text}
      <details class="text-muted-foreground w-full max-w-md text-xs">
        <summary class="cursor-pointer text-right">Text read from {image.name}</summary>
        <pre class="bg-muted mt-1 max-h-60 overflow-auto rounded-xl p-3 whitespace-pre-wrap">{image.text}</pre>
      </details>
    {:else if image.problem}
      <p class="text-muted-foreground max-w-md text-right text-xs">Text of {image.name} not read: {image.problem}.</p>
    {/if}
  {/each}
</div>
