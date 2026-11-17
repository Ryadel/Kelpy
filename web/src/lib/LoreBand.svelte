<script lang="ts">
  import { untrack } from 'svelte';
  import { fade } from 'svelte/transition';
  import EyeOffIcon from '@lucide/svelte/icons/eye-off';
  import XIcon from '@lucide/svelte/icons/x';
  import { Button } from '$lib/components/ui/button';
  import * as Dialog from '$lib/components/ui/dialog';
  import { nextFolkloreEntry, type FolkloreEntry } from './folklore';
  import { lore, showLore } from './lore.svelte';

  interface Props {
    /** An answer is being written in this conversation. */
    answering: boolean;
    /** The answer waits for the administrator to approve or deny an action: Kelpy is not thinking meanwhile. */
    waiting: boolean;
  }

  let { answering, waiting }: Props = $props();

  /** How long each passage stays, while the pointer is not on it. */
  const EVERY = 12_000;
  /** How long the last one stays once the answer has ended. */
  const LINGER = 4_000;
  /** How long the note on where to turn it back on stays. */
  const NOTE = 6_000;

  let entry = $state<FolkloreEntry | null>(null);
  /** The band is open: from the first passage of an answer until a little after it ends. */
  let open = $state(false);
  /** Closed for this answer with its ×. */
  let closed = $state(false);
  /** The pointer is on the band: the passage being read stays. */
  let pointed = false;
  let asking = $state(false);
  let noted = $state(false);
  let passage = $state<HTMLElement | null>(null);
  /** The current passage's height: outgoing passages overlap it while it changes size. */
  let height = $state(0);
  /** Counts the answers: a passage drawn for one that has ended is not shown. */
  let turn = 0;
  /** The answer of `turn` is still being written. */
  let live = false;

  // Hidden at once while an action waits for approval: the decision is the administrator's, and the band would say
  // Kelpy is still thinking.
  const visible = $derived(open && !closed && !waiting && lore.on && entry !== null);

  $effect(() => {
    if (!passage) {
      return;
    }
    const current = passage;
    const measure = () => (height = current.getBoundingClientRect().height);
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(current);
    return () => observer.disconnect();
  });

  function draw(forTurn: number) {
    void nextFolkloreEntry()
      .then((next) => {
        // The catalogue may load after the answer ended: the band is not opened again then.
        if (forTurn === turn && live) {
          entry = next;
          open = true;
        }
      })
      // Optional reading material must never get in the way of an answer.
      .catch(() => {});
  }

  // Each answer is a turn: its × holds until it ends, approvals included.
  $effect(() => {
    if (!answering) {
      return;
    }
    const mine = ++turn;
    live = true;
    untrack(() => (closed = false));
    return () => {
      live = false;
      setTimeout(() => {
        if (turn === mine) {
          open = false;
        }
      }, LINGER);
    };
  });

  // One passage when Kelpy starts thinking, at the start of an answer or after an approval, the next every few
  // seconds while it thinks. Declared after the turn's effect, so `turn` is already this answer's.
  $effect(() => {
    if (!answering || waiting) {
      return;
    }
    const mine = turn;
    untrack(() => {
      if (lore.on && !closed) {
        draw(mine);
      }
    });
    const cycle = setInterval(() => {
      if (lore.on && !closed && !pointed) {
        draw(mine);
      }
    }, EVERY);
    return () => clearInterval(cycle);
  });

  function turnOff() {
    asking = false;
    showLore(false);
    noted = true;
    setTimeout(() => (noted = false), NOTE);
  }
</script>

<div
  class="grid transition-[grid-template-rows,opacity] duration-500 ease-out {visible
    ? 'grid-rows-[1fr] opacity-100'
    : 'grid-rows-[0fr] opacity-0'}"
  inert={!visible}
>
  <div class="min-h-0 overflow-hidden">
    <div
      class="relative pt-1 pr-16 pb-4 pl-4"
      role="presentation"
      onpointerenter={() => (pointed = true)}
      onpointerleave={() => (pointed = false)}
    >
      <div class="relative transition-[height] duration-500 ease-out motion-reduce:transition-none" style:height="{height}px">
        {#if entry}
          {#key entry.id}
            <!-- The credit stays in sight, as the entries' licences ask. -->
            <figure
              bind:this={passage}
              class="border-primary/70 absolute inset-x-0 top-0 border-l-2 pl-3 [overflow-wrap:anywhere]"
              in:fade={{ duration: 700 }}
              out:fade={{ duration: 700 }}
            >
              <blockquote class="text-foreground/80 text-sm leading-relaxed italic">“{entry.text}”</blockquote>
              <figcaption class="text-muted-foreground mt-0.5 text-xs leading-relaxed">
                <span class="text-primary font-semibold">{entry.subject}</span>, {entry.tradition} ·
                <a href={entry.source} target="_blank" rel="noopener noreferrer" class="underline underline-offset-2">Source</a>
                · {entry.author} ·
                <a href={entry.licenseUrl} target="_blank" rel="noopener noreferrer" class="underline underline-offset-2">{entry.license}</a>
              </figcaption>
            </figure>
          {/key}
        {/if}
      </div>
      <div class="absolute top-0 right-0 flex">
        <Button
          variant="ghost"
          size="icon-sm"
          class="text-muted-foreground"
          title="Turn off the lore"
          aria-label="Turn off the lore"
          onclick={() => (asking = true)}
        >
          <EyeOffIcon class="size-3.5" />
        </Button>
        <Button
          variant="ghost"
          size="icon-sm"
          class="text-muted-foreground"
          title="Hide it until this answer ends"
          aria-label="Hide it until this answer ends"
          onclick={() => (closed = true)}
        >
          <XIcon class="size-3.5" />
        </Button>
      </div>
    </div>
  </div>
</div>
{#if noted}
  <p class="text-muted-foreground px-4 pb-2 text-xs" role="status" transition:fade>
    Lore turned off. Turn it back on from the menu at the top right.
  </p>
{/if}

<Dialog.Root bind:open={asking}>
  <Dialog.Content class="sm:max-w-md">
    <Dialog.Header>
      <Dialog.Title>Turn off the lore?</Dialog.Title>
      <Dialog.Description>
        While Kelpy thinks, short passages keep you company above the message box: creatures from the world's
        folklore, or from the Internet's own, each with its source.
      </Dialog.Description>
      <Dialog.Description>
        It stays off in this browser until you turn it back on from the menu at the top right:
        <span class="text-foreground font-medium">Show lore while Kelpy thinks</span>.
      </Dialog.Description>
    </Dialog.Header>
    <Dialog.Footer>
      <Button variant="outline" onclick={() => (asking = false)}>Cancel</Button>
      <Button onclick={turnOff}>Turn off</Button>
    </Dialog.Footer>
  </Dialog.Content>
</Dialog.Root>
