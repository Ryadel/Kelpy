<script lang="ts">
  import ChevronDownIcon from '@lucide/svelte/icons/chevron-down';
  import TriangleAlertIcon from '@lucide/svelte/icons/triangle-alert';
  import { Button } from '$lib/components/ui/button';
  import { cn } from '$lib/utils';
  import OperationDialog from './OperationDialog.svelte';
  import { diffLine, diffLines } from './actions';
  import { acceptDifference, checkDifferences, describe, putBack, type Action, type Difference } from './api';
  import { dateTime } from './dates';

  interface Props {
    differences: Difference[];
    /** The conversation shown: putting a file back is an operation in it. */
    sessionId: number;
    /** An operation ran: it joins the Command log. */
    onOperated: (action: Action) => void;
    onChanged: (differences: Difference[]) => void;
  }

  let { differences, sessionId, onOperated, onChanged }: Props = $props();

  let open = $state(false);
  let busy = $state(false);
  let error = $state<string | null>(null);
  let putting = $state<Difference | null>(null);
  let dialog = $state(false);

  const kinds: Record<Difference['kind'], string> = { file: 'File', service: 'Service', package: 'Package' };

  /** A digest is shortened; a state is shown as it is. */
  const shown = (state: string) => (state.startsWith('sha256:') ? `content ${state.slice(7, 19)}…` : state);

  async function accept(difference: Difference) {
    busy = true;
    error = null;
    try {
      await acceptDifference(difference.id);
      onChanged(differences.filter((other) => other.id !== difference.id));
    } catch (reason: unknown) {
      error = describe(reason);
    } finally {
      busy = false;
    }
  }

  async function check() {
    busy = true;
    error = null;
    try {
      onChanged(await checkDifferences());
    } catch (reason: unknown) {
      error = describe(reason);
    } finally {
      busy = false;
    }
  }

  function askPutBack(difference: Difference) {
    putting = difference;
    dialog = true;
  }
</script>

{#if differences.length > 0}
  <div class="mx-auto w-full max-w-3xl px-5 pt-3">
    <div class="rounded-xl border border-amber-500/40 bg-amber-500/5 text-sm">
      <button
        type="button"
        class="flex w-full items-center gap-2 px-4 py-2.5 text-left"
        aria-expanded={open}
        onclick={() => (open = !open)}
      >
        <TriangleAlertIcon class="size-4 shrink-0 text-amber-600 dark:text-amber-400" />
        <span class="font-medium">Unexpected changes</span>
        <span class="text-muted-foreground">
          {differences.length === 1
            ? 'One thing Kelpy changed is no longer as it left it.'
            : `${differences.length} things Kelpy changed are no longer as it left them.`}
        </span>
        <ChevronDownIcon class={cn('text-muted-foreground ml-auto size-4 shrink-0 transition-transform', open && 'rotate-180')} />
      </button>
      {#if open}
        <div class="max-h-[45vh] space-y-3 overflow-y-auto border-t border-amber-500/30 px-4 py-3">
          <p class="text-muted-foreground text-xs">
            Kelpy never reverts these by itself, and tells the AI model about them. Accept keeps what is there now;
            Put back writes Kelpy's version again, after a backup.
          </p>
          {#each differences as difference (difference.id)}
            <div class="bg-card rounded-lg border px-3 py-2.5">
              <div class="flex flex-wrap items-center gap-x-2 gap-y-1">
                <span class="text-muted-foreground text-xs">{kinds[difference.kind]}</span>
                <span class="min-w-0 font-mono text-xs [overflow-wrap:anywhere]">{difference.subject}</span>
                <span class="text-muted-foreground ml-auto text-xs">found {dateTime(difference.found_at)}</span>
              </div>
              {#if difference.diff}
                <pre class="bg-muted mt-2 max-h-60 overflow-auto rounded-lg py-1.5 font-mono text-xs">{#each diffLines(difference.diff) as line, index (index)}<span
                      class={cn('block px-3 whitespace-pre-wrap [overflow-wrap:anywhere]', diffLine(line))}>{line}</span
                    >{/each}</pre>
              {:else}
                <p class="mt-1.5 text-xs">
                  Expected <span class="font-mono">{shown(difference.expected)}</span>, found
                  <span class="font-mono">{shown(difference.found)}</span>.
                </p>
              {/if}
              <div class="mt-2 flex gap-2">
                <Button size="sm" variant="outline" disabled={busy} onclick={() => accept(difference)}>Accept</Button>
                {#if difference.can_put_back}
                  <Button size="sm" variant="outline" disabled={busy} onclick={() => askPutBack(difference)}>Put back</Button>
                {/if}
              </div>
            </div>
          {/each}
          <div class="flex items-center gap-3">
            <Button variant="link" size="sm" class="h-auto p-0 text-xs" disabled={busy} onclick={check}>Check again</Button>
            {#if error}<span class="text-destructive text-xs">{error}</span>{/if}
          </div>
        </div>
      {/if}
    </div>
  </div>
{/if}

{#if putting}
  {@const difference = putting}
  <OperationDialog
    bind:open={dialog}
    title="Put back Kelpy's version?"
    confirm="Put back"
    operate={(fingerprint) => putBack(sessionId, difference.id, fingerprint)}
    onDone={(action) => {
      onOperated(action);
      if (action.state === 'done') {
        onChanged(differences.filter((other) => other.id !== difference.id));
      }
    }}
  />
{/if}
