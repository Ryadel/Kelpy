<script lang="ts">
  import BanIcon from '@lucide/svelte/icons/ban';
  import CheckIcon from '@lucide/svelte/icons/check';
  import CircleAlertIcon from '@lucide/svelte/icons/circle-alert';
  import ClockIcon from '@lucide/svelte/icons/clock';
  import LoaderCircleIcon from '@lucide/svelte/icons/loader-circle';
  import XIcon from '@lucide/svelte/icons/x';
  import { Button } from '$lib/components/ui/button';
  import { cn } from '$lib/utils';
  import Elapsed from './Elapsed.svelte';
  import { duration, stateLabels } from './actions';
  import type { Action, Approval } from './api';

  interface Props {
    action: Action;
    /** Whether this page can take the decision, or stop the action, now. */
    canDecide: boolean;
    onDecide: (decision: 'approve' | 'deny', switchTo?: Approval) => Promise<void>;
    onStop: () => Promise<void>;
  }

  let { action, canDecide, onDecide, onStop }: Props = $props();

  let deciding = $state(false);
  let live = $state<HTMLPreElement>();
  /** Whether the output of a running command follows its end: the user has not scrolled up. */
  let following = true;

  const waiting = $derived(action.state === 'waiting');
  /** A command the model wrote: it always asks, whatever the Approval mode. */
  const written = $derived(action.tool === 'run_command');
  /** A command that ran and reported a failure. */
  const exitedBadly = $derived(action.state === 'done' && action.exit_code !== null && action.exit_code !== 0);
  const heading = $derived(
    waiting
      ? action.command
        ? 'Kelpy wants to run'
        : `Kelpy wants to ${action.summary.charAt(0).toLowerCase()}${action.summary.slice(1)}`
      : action.summary,
  );
  /** The state, unless done; the exit code; how long it ran, once it has. */
  const meta = $derived.by(() => {
    const parts = action.state === 'done' ? [] : [stateLabels[action.state]];
    if (action.exit_code !== null) {
      parts.push(`exit ${action.exit_code}`);
    }
    if (action.duration_ms !== null) {
      parts.push(duration(action.duration_ms));
    }
    return parts.join(' · ');
  });

  // A running command printed more: the view stays on its last line.
  $effect(() => {
    void action.output;
    if (live && following) {
      live.scrollTop = live.scrollHeight;
    }
  });

  function scrolled() {
    if (live) {
      following = live.scrollHeight - live.scrollTop - live.clientHeight < 16;
    }
  }

  async function decide(decision: 'approve' | 'deny', switchTo?: Approval) {
    deciding = true;
    try {
      await onDecide(decision, switchTo);
    } finally {
      deciding = false;
    }
  }

  async function stop() {
    deciding = true;
    try {
      await onStop();
    } finally {
      deciding = false;
    }
  }
</script>

<div class={cn('bg-card rounded-xl border px-4 py-3 text-sm', waiting && 'border-primary/50')}>
  <div class="flex items-center gap-2">
    {#if waiting}
      <ClockIcon class="text-primary size-4 shrink-0" />
    {:else if action.state === 'running'}
      <LoaderCircleIcon class="text-muted-foreground size-4 shrink-0 animate-spin" />
    {:else if action.state === 'failed'}
      <XIcon class="text-destructive size-4 shrink-0" />
    {:else if action.state === 'denied'}
      <BanIcon class="text-muted-foreground size-4 shrink-0" />
    {:else if exitedBadly}
      <CircleAlertIcon class="size-4 shrink-0 text-amber-600 dark:text-amber-400" />
    {:else}
      <CheckIcon class="text-primary size-4 shrink-0" />
    {/if}
    <span class="min-w-0 font-medium [overflow-wrap:anywhere]">{heading}</span>
    <span class="text-muted-foreground ml-auto shrink-0 text-xs">
      {meta}{#if action.state === 'running' && action.started_at !== null}{' · '}<Elapsed
          since={action.started_at}
        />{/if}
    </span>
  </div>
  {#if action.command}
    <pre class="bg-muted mt-2 overflow-x-auto rounded-lg px-3 py-2 font-mono text-xs whitespace-pre-wrap">{action.command}</pre>
  {/if}
  {#if action.reason}
    <p class="text-muted-foreground mt-2">{action.reason}</p>
  {/if}
  {#if action.error}
    <p class={cn('mt-2', action.state === 'failed' ? 'text-destructive' : 'text-muted-foreground')}>{action.error}</p>
  {/if}
  {#if waiting && written}
    <p class="text-muted-foreground mt-2 text-xs">
      Written by the AI model: Kelpy cannot check what it does. Read it before approving.
    </p>
  {/if}
  {#if waiting}
    <div class="mt-3 flex flex-wrap items-center gap-2">
      <Button size="sm" disabled={!canDecide || deciding} onclick={() => decide('approve')}>Approve</Button>
      <Button size="sm" variant="outline" disabled={!canDecide || deciding} onclick={() => decide('deny')}>Deny</Button>
      {#if !written}
        <span class="text-muted-foreground ml-auto flex items-center gap-1 text-xs">
          <Button
            variant="link"
            size="sm"
            class="h-auto p-0 text-xs"
            disabled={!canDecide || deciding}
            title="Approves this and runs what only looks without asking, in this conversation."
            onclick={() => decide('approve', 'plan')}>Switch to Plan</Button
          >
          ·
          <Button
            variant="link"
            size="sm"
            class="h-auto p-0 text-xs"
            disabled={!canDecide || deciding}
            title="Approves this and runs what only looks without asking, in this conversation."
            onclick={() => decide('approve', 'auto')}>Switch to Auto</Button
          >
        </span>
      {/if}
    </div>
  {/if}
  {#if action.state === 'running' && action.output}
    <pre
      bind:this={live}
      onscroll={scrolled}
      class="bg-muted mt-2 max-h-56 overflow-auto rounded-lg px-3 py-2 font-mono text-xs">{action.output}</pre>
  {:else if action.output}
    <details class="mt-2">
      <summary class="text-muted-foreground cursor-pointer text-xs select-none">Output</summary>
      <pre class="bg-muted mt-1.5 max-h-72 overflow-auto rounded-lg px-3 py-2 font-mono text-xs">{action.output}</pre>
    </details>
  {/if}
  {#if action.state === 'running'}
    <div class="mt-3">
      <Button
        size="sm"
        variant="outline"
        disabled={!canDecide || deciding}
        title="Stops this action only. Kelpy goes on with its answer."
        onclick={stop}>Stop</Button
      >
    </div>
  {/if}
</div>
