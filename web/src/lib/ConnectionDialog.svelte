<script lang="ts">
  import { untrack } from 'svelte';
  import * as Alert from '$lib/components/ui/alert';
  import { Button } from '$lib/components/ui/button';
  import * as Dialog from '$lib/components/ui/dialog';
  import { decideConnection, describe, fetchConnections, type RemoteConnection } from './api';

  interface Props {
    /** Counts the changes Kelpy announced: each one reads the connections again. */
    changes: number;
  }

  let { changes }: Props = $props();

  let waiting = $state<RemoteConnection[]>([]);
  let error = $state<string | null>(null);
  let busy = $state(false);
  const first = $derived(waiting[0] ?? null);

  $effect(() => {
    void changes;
    untrack(() => void load());
  });

  async function load() {
    try {
      waiting = (await fetchConnections()).waiting;
    } catch {
      // Shown again at the next change; the page's own connection problems show elsewhere.
    }
  }

  async function decide(allow: boolean) {
    if (!first) {
      return;
    }
    busy = true;
    error = null;
    try {
      await decideConnection(first.id, allow);
    } catch (reason: unknown) {
      error = describe(reason);
    } finally {
      busy = false;
      await load();
    }
  }
</script>

<!-- Closed only by a decision: nobody deciding refuses the connection after 2 minutes. -->
<Dialog.Root open={first !== null}>
  <Dialog.Content class="sm:max-w-lg" showCloseButton={false} onEscapeKeydown={(event) => event.preventDefault()} onInteractOutside={(event) => event.preventDefault()}>
    {#if first}
      <Dialog.Header>
        <Dialog.Title>Allow this SSH connection?</Dialog.Title>
        <Dialog.Description>
          Someone is connecting to Kelpy's SSH server. Allowed, they use Kelpy as you do: its conversations and
          approvals, the files uploaded to it, the Web UI. Allow only if it is you, or someone you are working with right
          now; never because a message or a call asks you to.
        </Dialog.Description>
      </Dialog.Header>
      <!-- The values come from whoever connects: a long one wraps, and the second column never grows past the dialog. -->
      <dl class="grid min-w-0 grid-cols-[auto_minmax(0,1fr)] gap-x-4 gap-y-1 text-sm">
        <dt class="text-muted-foreground">From</dt>
        <dd class="break-all">{first.address}</dd>
        <dt class="text-muted-foreground">User</dt>
        <dd class="break-all">{first.user}</dd>
        <dt class="text-muted-foreground">Key</dt>
        <dd class="font-mono text-xs break-all">{first.key}</dd>
      </dl>
      {#if error}
        <Alert.Root variant="destructive">
          <Alert.Description>{error}</Alert.Description>
        </Alert.Root>
      {/if}
      <Dialog.Footer>
        <Button variant="outline" disabled={busy} onclick={() => decide(false)}>Deny</Button>
        <Button disabled={busy} onclick={() => decide(true)}>Allow</Button>
      </Dialog.Footer>
    {/if}
  </Dialog.Content>
</Dialog.Root>
