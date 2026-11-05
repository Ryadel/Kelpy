<script lang="ts">
  import type { Component, Snippet } from 'svelte';
  import GlobeIcon from '@lucide/svelte/icons/globe';
  import HardDriveIcon from '@lucide/svelte/icons/hard-drive';

  interface Props {
    /** Puts a suggested request in the message box, to send or edit. */
    onSuggest: (text: string) => void;
    /** The message box, with any error above it. */
    children: Snippet;
  }

  let { onSuggest, children }: Props = $props();

  const suggestions: { icon: Component; text: string }[] = [
    { icon: GlobeIcon, text: 'Install WordPress with nginx and MariaDB' },
    { icon: HardDriveIcon, text: 'Why is the disk full?' },
  ];
</script>

<!-- The empty state of a conversation: shown only until its first message. -->
<div class="flex flex-1 flex-col items-center justify-center overflow-y-auto px-4 pb-[12vh]">
  <div class="w-full max-w-3xl">
    <div class="mb-7 flex flex-col items-center text-center">
      <span class="relative mb-4 inline-grid size-24 place-items-center">
        <span class="mark-glow -inset-8" aria-hidden="true"></span>
        <img src="/brand/kelpy-light.svg" alt="" width="96" height="96" class="relative size-24 object-contain" />
      </span>
      <h2 class="text-[28px] font-semibold tracking-tight">What do you need to do?</h2>
    </div>
    {@render children()}
    <div class="mt-5 flex flex-wrap justify-center gap-2">
      {#each suggestions as { icon: Icon, text } (text)}
        <button
          type="button"
          onclick={() => onSuggest(text)}
          class="text-muted-foreground hover:bg-muted hover:text-foreground inline-flex items-center gap-2 rounded-full border px-3.5 py-1.5 text-sm"
        >
          <Icon class="text-primary size-4" />
          {text}
        </button>
      {/each}
    </div>
  </div>
</div>
