<script lang="ts">
  import BanIcon from '@lucide/svelte/icons/ban';
  import CheckIcon from '@lucide/svelte/icons/check';
  import CircleIcon from '@lucide/svelte/icons/circle';
  import ListChecksIcon from '@lucide/svelte/icons/list-checks';
  import LoaderCircleIcon from '@lucide/svelte/icons/loader-circle';
  import ShieldAlertIcon from '@lucide/svelte/icons/shield-alert';
  import TriangleAlertIcon from '@lucide/svelte/icons/triangle-alert';
  import XIcon from '@lucide/svelte/icons/x';
  import { Button } from '$lib/components/ui/button';
  import { cn } from '$lib/utils';
  import { diffLine, stateLabels, type StepState } from './actions';
  import type { Action } from './api';

  interface Props {
    /** The action proposing the plan. */
    action: Action;
    /** Where each step is, from the actions that carried them out. */
    steps: StepState[];
    canDecide: boolean;
    onDecide: (decision: 'approve' | 'deny') => Promise<void>;
  }

  let { action, steps, canDecide, onDecide }: Props = $props();

  let deciding = $state(false);
  const waiting = $derived(action.state === 'waiting');
  const plan = $derived(action.plan!);
  const done = $derived(steps.filter((state) => state === 'done').length);

  async function decide(decision: 'approve' | 'deny') {
    deciding = true;
    try {
      await onDecide(decision);
    } finally {
      deciding = false;
    }
  }

  /** A step's detail: the lines a file change adds and removes are coloured, a command is not. */
  const changesFile = (tool: string) => tool === 'edit_file' || tool === 'write_file';
</script>

<div class={cn('bg-card rounded-xl border px-4 py-3 text-sm', waiting && 'border-primary/50')}>
  <div class="flex items-center gap-2">
    <ListChecksIcon class={cn('size-4 shrink-0', waiting ? 'text-primary' : 'text-muted-foreground')} />
    <span class="min-w-0 font-medium [overflow-wrap:anywhere]">
      {waiting ? 'Kelpy proposes a plan' : 'Plan'}: {plan.goal}
    </span>
    <span class="text-muted-foreground ml-auto shrink-0 text-xs">
      {#if action.state === 'done'}{done} of {plan.steps.length} done{:else}{stateLabels[action.state]}{/if}
    </span>
  </div>
  {#if action.reason}
    <p class="text-muted-foreground mt-2">{action.reason}</p>
  {/if}
  <ol class="mt-3 space-y-2">
    {#each plan.steps as step, index (index)}
      {@const state = steps[index]}
      <li class="flex gap-2">
        <span class="mt-0.5 shrink-0" title={state === 'pending' ? 'Not carried out yet' : stateLabels[state]}>
          {#if state === 'done'}
            <CheckIcon class="text-primary size-4" />
          {:else if state === 'running'}
            <LoaderCircleIcon class="text-muted-foreground size-4 animate-spin" />
          {:else if state === 'failed'}
            <XIcon class="text-destructive size-4" />
          {:else if state === 'uncertain'}
            <TriangleAlertIcon class="size-4 text-amber-600 dark:text-amber-400" />
          {:else if state === 'denied'}
            <BanIcon class="text-muted-foreground size-4" />
          {:else}
            <CircleIcon class="text-muted-foreground/60 size-4" />
          {/if}
        </span>
        <div class="min-w-0 flex-1">
          <p class="[overflow-wrap:anywhere]"><span class="text-muted-foreground">{index + 1}.</span> {step.summary}</p>
          {#if step.detail}
            <pre class="bg-muted mt-1 max-h-48 overflow-auto rounded-lg py-1.5 font-mono text-xs">{#each step.detail.split('\n') as line, number (number)}<span
                  class={cn(
                    'block px-3 whitespace-pre-wrap [overflow-wrap:anywhere]',
                    changesFile(step.tool) && diffLine(line),
                  )}>{line}</span
                >{/each}</pre>
          {/if}
          {#if step.floor}
            <p class="mt-1 flex gap-1.5 text-xs text-amber-700 dark:text-amber-400">
              <ShieldAlertIcon class="mt-px size-3.5 shrink-0" />{step.floor} Approving the plan approves it too.
            </p>
          {/if}
          {#if step.tool === 'run_command'}
            <p class="text-muted-foreground mt-1 text-xs">Written by the AI model: Kelpy cannot check what it does.</p>
          {/if}
        </div>
      </li>
    {/each}
  </ol>
  {#if action.error}
    <p class="text-muted-foreground mt-2">{action.error}</p>
  {/if}
  {#if waiting}
    <p class="text-muted-foreground mt-3 text-xs">
      Approving runs these steps without asking again, each once and only with these exact arguments. Anything else
      Kelpy asks for waits for your approval.
    </p>
    <div class="mt-3 flex flex-wrap items-center gap-2">
      <Button size="sm" disabled={!canDecide || deciding} onclick={() => decide('approve')}>Approve</Button>
      <Button size="sm" variant="outline" disabled={!canDecide || deciding} onclick={() => decide('deny')}>Deny</Button>
    </div>
  {/if}
</div>
