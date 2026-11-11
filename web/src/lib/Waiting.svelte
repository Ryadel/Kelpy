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

<div class="min-w-0 [overflow-wrap:anywhere]" role="status">
  <p class="text-muted-foreground flex items-center gap-2.5 text-sm">
    <span class="bg-primary size-2 shrink-0 rounded-full shadow-[0_0_10px_var(--primary)] motion-safe:animate-pulse"></span>
    Thinking…
  </p>
  {#if entry}
    <!-- The credit stays in sight, as the entries' licences ask. -->
    <figure class="border-primary mt-4 max-w-2xl border-l-2 py-0.5 pl-4">
      <blockquote class="text-[17px] leading-relaxed italic">“{entry.text}”</blockquote>
      <figcaption class="text-muted-foreground mt-2 text-[13px] leading-relaxed">
        <span class="text-primary font-semibold">{entry.subject}</span>, {entry.tradition} ·
        <a href={entry.source} target="_blank" rel="noopener noreferrer" class="underline underline-offset-2">Source</a>
        · {entry.author} ·
        <a href={entry.licenseUrl} target="_blank" rel="noopener noreferrer" class="underline underline-offset-2">{entry.license}</a>
      </figcaption>
    </figure>
  {/if}
</div>
