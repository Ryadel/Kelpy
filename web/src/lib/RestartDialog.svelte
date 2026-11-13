<script lang="ts">
  import LoaderCircleIcon from '@lucide/svelte/icons/loader-circle';
  import * as Alert from '$lib/components/ui/alert';
  import { Button } from '$lib/components/ui/button';
  import * as Dialog from '$lib/components/ui/dialog';
  import { cancelRestart, elevation, prompts, recordsOnly } from './elevation.svelte';

  const step = $derived(elevation.view?.step ?? 'prompt');
  const ended = $derived(elevation.failure !== null || elevation.lost);
  /** The memory switched to, for a restart asked from the Memory menu. */
  const memory = $derived(elevation.reason.kind === 'memory' ? elevation.reason.use : null);
  /** Only recorded as the one chosen, without a restart: shown when that failed. */
  const recording = $derived(memory !== null && recordsOnly(memory));
  /** Through the system's prompt: as administrator, or to the shared memory without the rights. Otherwise the restart
   * begins at once, also while the page still waits for Kelpy's answer. */
  const prompting = $derived(memory === null || prompts(memory));
  const title = $derived(
    memory === 'shared'
      ? 'Switching to the shared memory…'
      : memory === 'private'
        ? 'Switching to your private memory…'
        : 'Restarting as administrator…',
  );

  function close() {
    elevation.asked = false;
  }
</script>

<!-- Asked from this page: open until the page reloads onto the new Kelpy, or the restart did not happen. -->
<Dialog.Root
  open={elevation.asked}
  onOpenChange={(open) => {
    if (!open && ended) {
      close();
    }
  }}
>
  <Dialog.Content class="sm:max-w-md" showCloseButton={ended}>
    <Dialog.Header>
      <Dialog.Title class="flex items-center gap-2">
        {#if !ended}<LoaderCircleIcon class="text-primary size-4 animate-spin" />{/if}
        {ended ? (recording ? 'Kelpy did not record the choice' : 'Kelpy did not restart') : title}
      </Dialog.Title>
      <Dialog.Description>
        {#if elevation.lost}
          Kelpy did not come back. Start it again on this computer.
        {:else if elevation.failure !== null}
          {recording ? 'The memory chosen stays as it was.' : 'Kelpy goes on as it was.'}
        {:else if memory !== null && (step === 'restarting' || !prompting)}
          Kelpy restarts with {memory === 'shared' ? 'the shared memory' : 'your private memory'}, at the same
          address, with your conversations and sign-ins. This page reloads by itself.
        {:else if step === 'restarting'}
          Kelpy restarts with administrator rights, at the same address. This page reloads by itself.
        {:else if elevation.view?.how === 'terminal'}
          Type your password where Kelpy runs: sudo asks for it in Kelpy's terminal, and the password goes to sudo
          only. This page reloads by itself.
        {:else}
          {#if memory === 'shared'}Only administrators can open the shared memory.{/if}
          Confirm in the Windows prompt: <i>Do you want to allow this app to make changes to your device?</i> It may
          open behind this window. This page reloads by itself.
        {/if}
      </Dialog.Description>
    </Dialog.Header>
    {#if elevation.failure !== null}
      <Alert.Root variant="destructive">
        <Alert.Description>{elevation.failure}</Alert.Description>
      </Alert.Root>
    {/if}
    <Dialog.Footer>
      {#if ended}
        <Button variant="outline" onclick={close}>Close</Button>
      {:else if step === 'prompt'}
        <Button variant="outline" onclick={() => void cancelRestart()}>Cancel</Button>
      {/if}
    </Dialog.Footer>
  </Dialog.Content>
</Dialog.Root>
