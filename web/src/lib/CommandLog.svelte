<script lang="ts">
  import XIcon from '@lucide/svelte/icons/x';
  import { Button } from '$lib/components/ui/button';
  import { cn } from '$lib/utils';
  import Elapsed from './Elapsed.svelte';
  import { clock, duration, stateLabels } from './actions';
  import type { Action } from './api';

  interface Props {
    /** The actions of the open conversation, in order. */
    actions: Action[];
    onClose: () => void;
  }

  let { actions, onClose }: Props = $props();

  let list = $state<HTMLElement | null>(null);

  // A new entry shows at once, unless the user scrolled back to read an older one.
  $effect(() => {
    void actions.length;
    if (list && list.scrollHeight - list.scrollTop - list.clientHeight < 80) {
      list.scrollTop = list.scrollHeight;
    }
  });

  const stateColours: Record<Action['state'], string> = {
    waiting: 'text-primary',
    running: 'text-foreground',
    done: 'text-muted-foreground',
    failed: 'text-destructive',
    denied: 'text-muted-foreground',
  };
</script>

<aside
  class="bg-sidebar fixed inset-y-0 right-0 z-40 flex w-full max-w-sm flex-col border-l shadow-lg lg:static lg:z-auto lg:w-[380px] lg:max-w-none lg:shadow-none"
  aria-label="Command log"
>
  <div class="flex h-14 shrink-0 items-center gap-2 px-4">
    <h2 class="font-medium">Command log</h2>
    <Button
      variant="ghost"
      size="icon"
      class="text-muted-foreground ml-auto"
      onclick={onClose}
      aria-label="Close the Command log"
      title="Close the Command log"
    >
      <XIcon class="size-4" />
    </Button>
  </div>
  <ol class="min-h-0 flex-1 overflow-y-auto px-4 pb-4" bind:this={list}>
    {#each actions as action (action.id)}
      <li class="border-b py-2.5 text-xs last:border-b-0">
        <div class="text-muted-foreground flex flex-wrap items-center gap-x-2">
          <time>{clock(action.created_at)}</time>
          <span class={stateColours[action.state]}>{stateLabels[action.state]}</span>
          {#if action.exit_code !== null}
            <span class={cn(action.exit_code !== 0 && 'text-amber-600 dark:text-amber-400')}>exit {action.exit_code}</span>
          {/if}
          {#if action.duration_ms !== null}
            <span>{duration(action.duration_ms)}</span>
          {:else if action.state === 'running' && action.started_at !== null}
            <Elapsed since={action.started_at} />
          {/if}
        </div>
        {#if action.command}
          <pre class="mt-1 font-mono text-[12px] break-all whitespace-pre-wrap">{action.command}</pre>
        {:else}
          <p class="text-primary mt-1">{action.summary}</p>
        {/if}
        {#if action.error}
          <p class="text-muted-foreground mt-1">{action.error}</p>
        {/if}
        {#if action.output}
          <details class="mt-1">
            <summary class="text-muted-foreground cursor-pointer select-none">Output</summary>
            <pre class="bg-muted mt-1 max-h-64 overflow-auto rounded-md px-2 py-1.5 font-mono text-[11px]">{action.output}</pre>
          </details>
        {/if}
      </li>
    {:else}
      <p class="text-muted-foreground pt-2 text-sm">No commands in this conversation yet.</p>
    {/each}
  </ol>
</aside>
