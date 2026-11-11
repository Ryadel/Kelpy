<script lang="ts">
  import BanIcon from '@lucide/svelte/icons/ban';
  import CheckIcon from '@lucide/svelte/icons/check';
  import CircleAlertIcon from '@lucide/svelte/icons/circle-alert';
  import ShieldAlertIcon from '@lucide/svelte/icons/shield-alert';
  import ShieldCheckIcon from '@lucide/svelte/icons/shield-check';
  import ClockIcon from '@lucide/svelte/icons/clock';
  import LoaderCircleIcon from '@lucide/svelte/icons/loader-circle';
  import TriangleAlertIcon from '@lucide/svelte/icons/triangle-alert';
  import XIcon from '@lucide/svelte/icons/x';
  import { Button } from '$lib/components/ui/button';
  import { cn } from '$lib/utils';
  import Elapsed from './Elapsed.svelte';
  import NoteText from './NoteText.svelte';
  import { diffLine, diffLines as linesOf, duration, stateLabels } from './actions';
  import type { Action, Approval } from './api';
  import { canRestart, elevation, restartAsAdministrator } from './elevation.svelte';
  import { failedForRights } from './rights';
  import { runtime } from './runtime.svelte';

  interface Props {
    action: Action;
    /** The conversation's Approval mode: the card offers only the modes that would change it. */
    approval: Approval;
    /** Whether this page can take the decision, or stop the action, now. */
    canDecide: boolean;
    onDecide: (decision: 'approve' | 'deny', switchTo?: Approval) => Promise<void>;
    onStop: () => Promise<void>;
  }

  let { action, approval, canDecide, onDecide, onStop }: Props = $props();

  let deciding = $state(false);

  /** Failed for lack of rights, with Kelpy not elevated: the restart as administrator is offered under it. */
  const offerRestart = $derived(
    failedForRights(action) &&
      elevation.view !== null &&
      !elevation.view.elevated &&
      elevation.view.how !== null &&
      !elevation.dismissed.includes(action.id),
  );
  const offerText = $derived.by(() => {
    const account = runtime.status?.account ?? 'its own account';
    switch (elevation.view?.how) {
      case 'prompt':
        return `Kelpy runs as ${account}. Restarted as administrator, it can do it: Windows asks you to confirm. This page, the conversations, keys and secrets stay as they are.`;
      case 'terminal':
        return `Kelpy runs as ${account}. Restarted as root, it can do it: sudo asks for your password in Kelpy's terminal. This page, the conversations, keys and secrets stay as they are.`;
      default:
        return `Kelpy runs as ${account}, without root. To do it, stop Kelpy and start it again with sudo kelpy.`;
    }
  });
  let live = $state<HTMLPreElement>();
  /** Whether the output of a running command follows its end: the user has not scrolled up. */
  let following = true;

  const waiting = $derived(action.state === 'waiting');
  /** A command the model wrote: it always asks, whatever the Approval mode. */
  const written = $derived(action.tool === 'run_command');
  /** Plan helps from Ask only; Auto from Ask or Plan. A command written or a change on the floor asks in every mode. */
  const toPlan = $derived(!written && !action.change?.floor && approval === 'ask');
  const toAuto = $derived(!written && !action.change?.floor && approval !== 'auto');
  /** The diff's lines, without its two header lines: the path is in the heading. */
  const diffLines = $derived(linesOf(action.change?.diff ?? ''));
  /** A command that ran and reported a failure. */
  const exitedBadly = $derived(action.state === 'done' && action.exit_code !== null && action.exit_code !== 0);
  /** An inspection shows its command; a change says what it does, its commands below. */
  const heading = $derived(
    waiting
      ? action.command && !written && !action.change
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
    {:else if action.state === 'uncertain'}
      <TriangleAlertIcon class="size-4 shrink-0 text-amber-600 dark:text-amber-400" />
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
  {#if action.change}
    <pre class="bg-muted mt-2 max-h-80 overflow-auto rounded-lg py-2 font-mono text-xs">{#each diffLines as line, index (index)}<span
          class={cn('block px-3 whitespace-pre-wrap [overflow-wrap:anywhere]', diffLine(line))}>{line}</span
        >{/each}</pre>
  {/if}
  {#if action.reason}
    <p class="text-muted-foreground mt-2">{action.reason}</p>
  {/if}
  {#if action.change && waiting}
    <ul class="text-muted-foreground mt-2 space-y-0.5 text-xs">
      {#each action.change.notes as note (note)}
        <li><NoteText text={note} /></li>
      {/each}
    </ul>
  {/if}
  {#if action.change?.floor}
    <p class="mt-2 flex gap-1.5 text-xs text-amber-700 dark:text-amber-400">
      <ShieldAlertIcon class="mt-px size-3.5 shrink-0" />{action.change.floor}
    </p>
  {/if}
  {#if action.state === 'running' && action.phase}
    <p class="text-muted-foreground mt-2 text-xs">Now {action.phase}…</p>
  {/if}
  {#if action.error}
    <p
      class={cn(
        'mt-2',
        action.state === 'failed'
          ? 'text-destructive'
          : action.state === 'uncertain'
            ? 'text-amber-700 dark:text-amber-400'
            : 'text-muted-foreground',
      )}
    >
      {action.error}{#if action.state === 'uncertain'}. Kelpy will not do it again by itself: check the system
        first.{/if}
    </p>
  {/if}
  {#if offerRestart}
    <div class="border-primary/40 bg-primary/5 mt-3 flex gap-3 rounded-lg border p-3">
      <ShieldCheckIcon class="text-primary mt-0.5 size-4 shrink-0" />
      <div class="grid gap-1">
        <p class="font-medium">This needs administrator rights</p>
        <p class="text-muted-foreground text-sm">{offerText}</p>
        {#if canRestart()}
          <div class="mt-1.5 flex flex-wrap gap-2">
            <Button size="sm" onclick={() => void restartAsAdministrator()}>
              <ShieldCheckIcon class="size-4" /> Restart as administrator
            </Button>
            <Button size="sm" variant="outline" onclick={() => elevation.dismissed.push(action.id)}>Not now</Button>
          </div>
        {/if}
      </div>
    </div>
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
      {#if toPlan || toAuto}
        <span class="text-muted-foreground ml-auto flex items-center gap-1 text-xs">
          {#if toPlan}
            <Button
              variant="link"
              size="sm"
              class="h-auto p-0 text-xs"
              disabled={!canDecide || deciding}
              title="Approves this and, in this conversation, runs what only looks without asking; changes come as one plan to approve."
              onclick={() => decide('approve', 'plan')}>Switch to Plan</Button
            >
          {/if}
          {#if toPlan && toAuto}·{/if}
          {#if toAuto}
            <Button
              variant="link"
              size="sm"
              class="h-auto p-0 text-xs"
              disabled={!canDecide || deciding}
              title="Approves this and, in this conversation, runs without asking what only looks and the changes Kelpy checks and can put back."
              onclick={() => decide('approve', 'auto')}>Switch to Auto</Button
            >
          {/if}
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
