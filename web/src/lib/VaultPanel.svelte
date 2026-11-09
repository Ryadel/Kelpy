<script lang="ts">
  import LockIcon from '@lucide/svelte/icons/lock';
  import LockOpenIcon from '@lucide/svelte/icons/lock-open';
  import * as Alert from '$lib/components/ui/alert';
  import { Button } from '$lib/components/ui/button';
  import { Input } from '$lib/components/ui/input';
  import {
    addUnlock,
    createVault,
    deleteVault,
    lockVault,
    removeUnlock,
    unlockVault,
    type SecretsView,
    type UnlockRequest,
  } from './api';
  import UnlockForm from './UnlockForm.svelte';

  interface Props {
    view: SecretsView;
    busy: boolean;
    /** Runs a change of the vault, then reads the secrets again. */
    onAct: (work: () => Promise<void>) => void;
  }

  let { view, busy, onAct }: Props = $props();

  const vault = $derived(view.vault);
  /** The form open: creating the vault, or adding a way to unlock it. */
  let form = $state<'create' | 'add' | null>(null);
  let confirmingDelete = $state(false);
  let passphrase = $state('');
  const hasPassphrase = $derived(vault.unlocks.some((unlock) => unlock.kind === 'passphrase'));
  /** The keys of the agent that unlock the vault. */
  const agentUnlocks = $derived(
    vault.unlocks.filter(
      (unlock) =>
        unlock.kind === 'ssh_key' && view.agent_keys.some((key) => key.fingerprint === unlock.fingerprint),
    ),
  );

  function submit(unlock: UnlockRequest) {
    const creating = form === 'create';
    onAct(async () => {
      await (creating ? createVault(unlock) : addUnlock(unlock));
      form = null;
    });
  }

  function unlockWithPassphrase(event: SubmitEvent) {
    event.preventDefault();
    onAct(async () => {
      await unlockVault({ method: 'passphrase', passphrase });
      passphrase = '';
    });
  }
</script>

<section class="grid grid-cols-[minmax(0,1fr)] gap-3" aria-label="Vault">
  <div class="flex flex-wrap items-center gap-2">
    {#if vault.state === 'unlocked'}
      <LockOpenIcon class="text-primary size-4" />
    {:else}
      <LockIcon class="text-muted-foreground size-4" />
    {/if}
    <h3 class="font-medium">Vault</h3>
    <span class="text-muted-foreground text-sm">
      {vault.problem ? 'cannot be read' : vault.state === 'none' ? 'not created' : vault.state}
    </span>
    {#if vault.state === 'unlocked'}
      <Button variant="outline" size="sm" class="ml-auto" disabled={busy} onclick={() => onAct(lockVault)}>Lock</Button>
    {/if}
  </div>

  {#if vault.problem}
    <Alert.Root variant="destructive">
      <Alert.Description>
        The vault cannot be read: {vault.problem}. Delete it to create a new one; the secrets kept in it are lost.
      </Alert.Description>
    </Alert.Root>
  {:else if vault.state === 'none'}
    <p class="text-muted-foreground text-sm">
      Secrets you add last until Kelpy stops. A vault keeps them across restarts, encrypted in Kelpy's data folder; it
      opens with a passphrase or with an SSH key of the agent of Kelpy's session.
    </p>
    {#if form === 'create'}
      <UnlockForm
        action="Create the vault"
        agentKeys={view.agent_keys}
        agentProblem={view.agent_problem}
        {busy}
        onSubmit={submit}
        onCancel={() => (form = null)}
      />
    {:else}
      <div><Button variant="outline" onclick={() => (form = 'create')}>Create a vault</Button></div>
    {/if}
  {:else if vault.state === 'locked'}
    <p class="text-muted-foreground text-sm">
      Locked: the secrets kept in it are missing, for you and for the model, until it is unlocked. It locks again when
      Kelpy stops.
    </p>
    {#if hasPassphrase}
      <form class="flex gap-2" onsubmit={unlockWithPassphrase}>
        <Input type="password" bind:value={passphrase} placeholder="Passphrase" aria-label="Passphrase" autocomplete="current-password" />
        <Button type="submit" disabled={busy || passphrase === ''}>Unlock</Button>
      </form>
    {/if}
    {#each agentUnlocks as unlock (unlock.kind === 'ssh_key' ? unlock.fingerprint : '')}
      {#if unlock.kind === 'ssh_key'}
        <div>
          <Button
            variant="outline"
            class="h-auto max-w-full text-left break-all whitespace-normal"
            disabled={busy}
            onclick={() => onAct(() => unlockVault({ method: 'ssh_key', fingerprint: unlock.fingerprint }))}
          >
            Unlock with {unlock.label}
          </Button>
        </div>
      {/if}
    {/each}
  {:else}
    <div class="grid grid-cols-[minmax(0,1fr)] gap-1.5">
      <p class="text-muted-foreground text-sm">Opens with:</p>
      <ul class="grid grid-cols-[minmax(0,1fr)] gap-1">
        {#each vault.unlocks as unlock, index (index)}
          <li class="flex flex-wrap items-center gap-x-2 gap-y-1 text-sm">
            <span class="min-w-0 truncate">{unlock.kind === 'passphrase' ? 'Passphrase' : `SSH key ${unlock.label}`}</span>
            <Button
              variant="ghost"
              size="sm"
              class="ml-auto"
              disabled={busy || vault.unlocks.length === 1}
              title={vault.unlocks.length === 1 ? 'The last way to unlock cannot be removed' : undefined}
              onclick={() => onAct(() => removeUnlock(index))}
            >
              Remove
            </Button>
          </li>
        {/each}
      </ul>
      {#if form === 'add'}
        <UnlockForm
          action="Add"
          agentKeys={view.agent_keys}
          agentProblem={view.agent_problem}
          {busy}
          onSubmit={submit}
          onCancel={() => (form = null)}
        />
      {:else}
        <div><Button variant="outline" size="sm" onclick={() => (form = 'add')}>Add a way to unlock</Button></div>
      {/if}
    </div>
  {/if}

  {#if vault.problem || vault.state !== 'none'}
    {#if confirmingDelete}
      <Alert.Root>
        <Alert.Description class="grid gap-2">
          Delete the vault? The secrets kept in it are lost here; where Kelpy wrote them on the server, they stay.
          <div class="flex justify-end gap-2">
            <Button variant="ghost" size="sm" onclick={() => (confirmingDelete = false)}>Cancel</Button>
            <Button
              variant="destructive"
              size="sm"
              disabled={busy}
              onclick={() =>
                onAct(async () => {
                  await deleteVault();
                  confirmingDelete = false;
                })}
            >
              Delete the vault
            </Button>
          </div>
        </Alert.Description>
      </Alert.Root>
    {:else}
      <div>
        <Button variant="link" size="sm" class="text-muted-foreground px-0" onclick={() => (confirmingDelete = true)}>
          Delete the vault
        </Button>
      </div>
    {/if}
  {/if}
</section>
