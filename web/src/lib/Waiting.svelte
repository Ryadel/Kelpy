<script lang="ts">
  import { onMount } from 'svelte';
  import { nextFolkloreEntry, type FolkloreEntry } from './folklore';

  let entry = $state<FolkloreEntry | null>(null);

  onMount(() => {
    let mounted = true;
    void nextFolkloreEntry()
      .then((selected) => {
        if (mounted) entry = selected;
      })
      // Optional reading material must never prevent an answer from appearing.
      .catch(() => {});
    return () => { mounted = false; };
  });
</script>

<div class="text-muted-foreground min-w-0 [overflow-wrap:anywhere]" role="status">
  <p class="motion-safe:animate-pulse text-sm">Thinking…</p>
  {#if entry}
    <p class="mt-2 text-sm leading-relaxed">{entry.text}</p>
    <p class="mt-1 text-xs leading-relaxed">
      <a href={entry.source} target="_blank" rel="noopener noreferrer" class="underline underline-offset-2">{entry.subject}</a>
      · {entry.tradition} · {entry.author} ·
      <a href={entry.licenseUrl} target="_blank" rel="noopener noreferrer" class="underline underline-offset-2">{entry.license}</a>
    </p>
  {/if}
</div>
