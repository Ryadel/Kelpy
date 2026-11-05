<script lang="ts">
  import DownloadIcon from '@lucide/svelte/icons/download';
  import FileTextIcon from '@lucide/svelte/icons/file-text';
  import XIcon from '@lucide/svelte/icons/x';
  import { Button } from '$lib/components/ui/button';
  import { cn } from '$lib/utils';
  import Elapsed from './Elapsed.svelte';
  import OperationDialog from './OperationDialog.svelte';
  import { clock, duration, restorable, stateLabels } from './actions';
  import { describe, fetchExport, fetchReport, restoreAction, type Action } from './api';
  import { download } from './download';

  interface Props {
    /** Every action of the open conversation, in order: the answers' and the administrator's. */
    actions: Action[];
    sessionId: number;
    /** The conversation's title, for the names of the files downloaded. */
    title: string;
    /** An operation of the administrator ran: it joins the log. */
    onOperated: (action: Action) => void;
    onClose: () => void;
  }

  let { actions, sessionId, title, onOperated, onClose }: Props = $props();

  let list = $state<HTMLElement | null>(null);
  let restoring = $state<Action | null>(null);
  let dialog = $state(false);
  let error = $state<string | null>(null);

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
    uncertain: 'text-amber-600 dark:text-amber-400',
  };

  /** The conversation's title as part of a file's name. */
  const named = $derived(title.replace(/[^\p{L}\p{N}]+/gu, '-').replace(/^-|-$/g, '').slice(0, 60) || 'conversation');

  async function saveReport() {
    error = null;
    try {
      download(`kelpy-report-${named}.md`, await fetchReport(sessionId), 'text/markdown');
    } catch (reason: unknown) {
      error = describe(reason);
    }
  }

  async function saveExport() {
    error = null;
    try {
      download(`kelpy-operations-${named}.json`, await fetchExport(sessionId), 'application/json');
    } catch (reason: unknown) {
      error = describe(reason);
    }
  }

  function askRestore(action: Action) {
    restoring = action;
    dialog = true;
  }
</script>

<aside
  class="bg-sidebar fixed inset-y-0 right-0 z-40 flex w-full max-w-sm flex-col border-l shadow-lg lg:static lg:z-auto lg:w-[380px] lg:max-w-none lg:shadow-none"
  aria-label="Command log"
>
  <div class="flex h-14 shrink-0 items-center gap-1 px-4">
    <h2 class="mr-auto font-medium">Command log</h2>
    <Button
      variant="ghost"
      size="sm"
      class="text-muted-foreground h-8 gap-1.5 px-2 text-xs"
      title="Downloads the handover report: what changed, what did not complete, where secrets went, what stays on the server"
      onclick={saveReport}><FileTextIcon class="size-3.5" />Report</Button
    >
    <Button
      variant="ghost"
      size="sm"
      class="text-muted-foreground h-8 gap-1.5 px-2 text-xs"
      title="Downloads the operations that succeeded, with their arguments and checks, as JSON"
      onclick={saveExport}><DownloadIcon class="size-3.5" />Export</Button
    >
    <Button
      variant="ghost"
      size="icon"
      class="text-muted-foreground"
      onclick={onClose}
      aria-label="Close the Command log"
      title="Close the Command log"
    >
      <XIcon class="size-4" />
    </Button>
  </div>
  {#if error}
    <p class="text-destructive px-4 pb-2 text-xs">{error}</p>
  {/if}
  <ol class="min-h-0 flex-1 overflow-y-auto px-4 pb-4" bind:this={list}>
    {#each actions as action (action.id)}
      <li class="border-b py-2.5 text-xs last:border-b-0">
        <div class="text-muted-foreground flex flex-wrap items-center gap-x-2">
          <time>{clock(action.created_at)}</time>
          {#if action.message_id === null}
            <span class="text-foreground">You</span>
          {/if}
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
          {#if action.change}
            <p class="text-primary mt-1">{action.summary}</p>
          {/if}
        {:else}
          <p class="text-primary mt-1">{action.summary}</p>
        {/if}
        {#if action.error}
          <p class="text-muted-foreground mt-1">{action.error}</p>
        {/if}
        {#if action.change?.backup}
          <p class="text-muted-foreground mt-1 break-all">Backup: {action.change.backup}</p>
        {/if}
        <div class="flex items-center gap-3">
          {#if action.output}
            <details class="mt-1 min-w-0 flex-1">
              <summary class="text-muted-foreground cursor-pointer select-none">Output</summary>
              <pre class="bg-muted mt-1 max-h-64 overflow-auto rounded-md px-2 py-1.5 font-mono text-[11px]">{action.output}</pre>
            </details>
          {/if}
          {#if restorable(action)}
            <Button
              variant="link"
              size="sm"
              class="mt-1 ml-auto h-auto self-start p-0 text-xs"
              title="Puts the file back as it was before this change, from its backup, after backing up the version it replaces"
              onclick={() => askRestore(action)}>Restore</Button
            >
          {/if}
        </div>
      </li>
    {:else}
      <p class="text-muted-foreground pt-2 text-sm">No commands in this conversation yet.</p>
    {/each}
  </ol>
</aside>

{#if restoring}
  {@const action = restoring}
  <OperationDialog
    bind:open={dialog}
    title="Restore the file?"
    confirm="Restore"
    operate={(fingerprint) => restoreAction(sessionId, action.id, fingerprint)}
    onDone={onOperated}
  />
{/if}
