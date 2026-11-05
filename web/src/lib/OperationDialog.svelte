<script lang="ts">
  import ShieldAlertIcon from '@lucide/svelte/icons/shield-alert';
  import * as Alert from '$lib/components/ui/alert';
  import { Button } from '$lib/components/ui/button';
  import * as Dialog from '$lib/components/ui/dialog';
  import { cn } from '$lib/utils';
  import { diffLine, diffLines } from './actions';
  import { describe, type Action, type Change, type Operated } from './api';

  interface Props {
    open: boolean;
    /** The dialog's title, such as "Restore the file?". */
    title: string;
    /** The button that runs it. */
    confirm: string;
    /** Asks Kelpy what the operation would change (`null`), or runs the change shown (its fingerprint). */
    operate: (fingerprint: string | null) => Promise<Operated>;
    /** Called with the action once it ran, or failed. */
    onDone: (action: Action) => void;
  }

  let { open = $bindable(), title, confirm, operate, onDone }: Props = $props();

  let preview = $state<{ summary: string; change: Change; fingerprint: string } | null>(null);
  let error = $state<string | null>(null);
  let working = $state(false);

  // Each opening shows what the operation would change now.
  $effect(() => {
    if (open) {
      void show();
    } else {
      preview = null;
      error = null;
    }
  });

  async function show() {
    preview = null;
    error = null;
    try {
      const operated = await operate(null);
      if (operated.outcome === 'preview') {
        preview = operated;
      }
    } catch (reason: unknown) {
      error = describe(reason);
    }
  }

  async function run() {
    if (!preview) return;
    working = true;
    error = null;
    try {
      const operated = await operate(preview.fingerprint);
      if (operated.outcome === 'done') {
        onDone(operated.action);
        if (operated.action.state === 'done') {
          open = false;
        } else {
          error = operated.action.error ?? 'It did not complete.';
        }
      }
    } catch (reason: unknown) {
      error = describe(reason);
    } finally {
      working = false;
    }
  }
</script>

<Dialog.Root bind:open>
  <Dialog.Content class="sm:max-w-2xl">
    <Dialog.Header>
      <Dialog.Title>{title}</Dialog.Title>
      <Dialog.Description>
        {#if preview}{preview.summary}.{:else if !error}Kelpy reads the file to show what changes…{/if}
      </Dialog.Description>
    </Dialog.Header>
    {#if preview}
      <pre class="bg-muted max-h-80 overflow-auto rounded-lg py-2 font-mono text-xs">{#each diffLines(preview.change.diff) as line, index (index)}<span
            class={cn('block px-3 whitespace-pre-wrap [overflow-wrap:anywhere]', diffLine(line))}>{line}</span
          >{/each}</pre>
      <ul class="text-muted-foreground space-y-0.5 text-xs">
        {#each preview.change.notes as note (note)}
          <li>{note}</li>
        {/each}
        <li>Your own action: it does not depend on the conversation's System access or Approval mode.</li>
      </ul>
      {#if preview.change.floor}
        <p class="flex gap-1.5 text-xs text-amber-700 dark:text-amber-400">
          <ShieldAlertIcon class="mt-px size-3.5 shrink-0" />{preview.change.floor}
        </p>
      {/if}
    {/if}
    {#if error}
      <Alert.Root variant="destructive">
        <Alert.Description>{error}</Alert.Description>
      </Alert.Root>
    {/if}
    <Dialog.Footer>
      <Button variant="outline" onclick={() => (open = false)}>Cancel</Button>
      <Button onclick={run} disabled={!preview || working}>{confirm}</Button>
    </Dialog.Footer>
  </Dialog.Content>
</Dialog.Root>
