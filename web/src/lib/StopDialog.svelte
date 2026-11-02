<script lang="ts">
  import * as Alert from '$lib/components/ui/alert';
  import { Button } from '$lib/components/ui/button';
  import * as Dialog from '$lib/components/ui/dialog';
  import { describe, stopKelpy } from './api';

  interface Props {
    open: boolean;
    onStopped: () => void;
  }

  let { open = $bindable(), onStopped }: Props = $props();

  let stopping = $state(false);
  let error = $state<string | null>(null);

  async function stop() {
    stopping = true;
    error = null;
    try {
      await stopKelpy();
      open = false;
      onStopped();
    } catch (reason: unknown) {
      error = describe(reason);
    } finally {
      stopping = false;
    }
  }
</script>

<Dialog.Root bind:open>
  <Dialog.Content class="sm:max-w-md">
    <Dialog.Header>
      <Dialog.Title>Stop Kelpy?</Dialog.Title>
      <Dialog.Description>
        Kelpy stops on this server and this page stops working. Answers being written are stopped and kept as
        incomplete; conversations stay saved. To use Kelpy again, start it on the server.
      </Dialog.Description>
    </Dialog.Header>
    {#if error}
      <Alert.Root variant="destructive">
        <Alert.Description>{error}</Alert.Description>
      </Alert.Root>
    {/if}
    <Dialog.Footer>
      <Button variant="outline" onclick={() => (open = false)}>Cancel</Button>
      <Button variant="destructive" onclick={stop} disabled={stopping}>Stop Kelpy</Button>
    </Dialog.Footer>
  </Dialog.Content>
</Dialog.Root>
