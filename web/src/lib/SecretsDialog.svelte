<script lang="ts">
  import { untrack } from 'svelte';
  import * as Alert from '$lib/components/ui/alert';
  import { Badge } from '$lib/components/ui/badge';
  import { Button } from '$lib/components/ui/button';
  import * as Dialog from '$lib/components/ui/dialog';
  import { Input } from '$lib/components/ui/input';
  import { Label } from '$lib/components/ui/label';
  import {
    addSecret,
    describe,
    fetchSecrets,
    keepSecret,
    removeSecret,
    secretValue,
    type SecretsView,
    type Session,
  } from './api';
  import { copyText } from './clipboard';
  import VaultPanel from './VaultPanel.svelte';

  interface Props {
    open: boolean;
    sessions: Session[];
    /** Counts the changes Kelpy announced: each one reads the secrets again. */
    changes: number;
  }

  let { open = $bindable(), sessions, changes }: Props = $props();

  let view = $state<SecretsView | null>(null);
  let error = $state<string | null>(null);
  let busy = $state(false);
  /** The secret just copied, as `name` or `session:name`. */
  let copied = $state<string | null>(null);
  let name = $state('');
  let value = $state('');
  let keep = $state(true);

  const unlocked = $derived(view?.vault.state === 'unlocked');
  const title = (id: number) => sessions.find((session) => session.id === id)?.title ?? `Conversation ${id}`;

  $effect(() => {
    if (open) {
      void changes;
      untrack(() => void load());
    }
  });

  async function load() {
    try {
      view = await fetchSecrets();
    } catch (reason: unknown) {
      error = describe(reason);
    }
  }

  /** Runs `work`, then reads the secrets again; its error shows above them. */
  async function act(work: () => Promise<void>) {
    busy = true;
    error = null;
    try {
      await work();
    } catch (reason: unknown) {
      error = describe(reason);
    } finally {
      busy = false;
      await load();
    }
  }

  async function copy(secret: string, sessionId: number | null) {
    error = null;
    try {
      if (await copyText(await secretValue(secret, sessionId))) {
        const key = sessionId === null ? secret : `${sessionId}:${secret}`;
        copied = key;
        setTimeout(() => copied === key && (copied = null), 1500);
      } else {
        error = 'This browser does not allow copying from this page.';
      }
    } catch (reason: unknown) {
      error = describe(reason);
    }
  }

  function add(event: SubmitEvent) {
    event.preventDefault();
    void act(async () => {
      await addSecret(name.trim(), value, keep && unlocked);
      name = '';
      value = '';
    });
  }
</script>

<Dialog.Root bind:open>
  <Dialog.Content class="max-h-[88dvh] overflow-y-auto sm:max-w-xl">
    <Dialog.Header>
      <Dialog.Title>Secrets</Dialog.Title>
      <Dialog.Description>
        Passwords and keys Kelpy writes where they go, in files and database users. The model knows them by name only,
        as <code>{'{{secret:name}}'}</code>; their values show only here.
      </Dialog.Description>
    </Dialog.Header>

    {#if error}
      <Alert.Root variant="destructive">
        <Alert.Description>{error}</Alert.Description>
      </Alert.Root>
    {/if}

    {#if view}
      <VaultPanel {view} {busy} onAct={(work) => void act(work)} />

      <section class="grid grid-cols-[minmax(0,1fr)] gap-2 border-t pt-4" aria-label="Your secrets">
        <h3 class="font-medium">Your secrets</h3>
        <p class="text-muted-foreground text-sm">Every conversation can use them.</p>
        {#if view.shared.length === 0}
          <p class="text-muted-foreground text-sm">None.</p>
        {/if}
        <ul class="grid grid-cols-[minmax(0,1fr)] gap-1">
          {#each view.shared as secret (secret.name)}
            <li class="flex flex-wrap items-center gap-x-2 gap-y-1 text-sm">
              <code class="min-w-0 truncate">{secret.name}</code>
              <Badge variant="outline">{secret.kept ? 'In the vault' : 'Until Kelpy stops'}</Badge>
              <span class="ml-auto flex shrink-0 gap-1">
                <Button variant="ghost" size="sm" onclick={() => copy(secret.name, null)}>
                  {copied === secret.name ? 'Copied' : 'Copy'}
                </Button>
                {#if unlocked && !secret.kept}
                  <Button variant="ghost" size="sm" disabled={busy} onclick={() => act(() => keepSecret(secret.name, null))}>
                    Keep in the vault
                  </Button>
                {/if}
                <Button variant="ghost" size="sm" disabled={busy} onclick={() => act(() => removeSecret(secret.name))}>
                  Remove
                </Button>
              </span>
            </li>
          {/each}
        </ul>
        <form class="grid grid-cols-[minmax(0,1fr)] gap-2 sm:grid-cols-[minmax(0,1fr)_minmax(0,1.4fr)_auto] sm:items-end" onsubmit={add}>
          <div class="grid gap-1.5">
            <Label for="secret-name">Name</Label>
            <Input id="secret-name" bind:value={name} autocomplete="off" pattern={'[A-Za-z0-9_]{1,64}'} title="Letters, digits and _" />
          </div>
          <div class="grid gap-1.5">
            <Label for="secret-value">Value</Label>
            <Input id="secret-value" type="password" bind:value autocomplete="off" />
          </div>
          <Button type="submit" disabled={busy || name.trim() === '' || value === ''}>Add</Button>
          <label class="text-muted-foreground flex items-center gap-2 text-sm sm:col-span-3">
            <input
              type="checkbox"
              checked={keep && unlocked}
              disabled={!unlocked}
              onchange={(event) => (keep = event.currentTarget.checked)}
            />
            {unlocked ? 'Keep in the vault' : 'Keep in the vault (unlock or create it first)'}
          </label>
        </form>
      </section>

      {#if view.generated.length > 0}
        <section class="grid grid-cols-[minmax(0,1fr)] gap-2 border-t pt-4" aria-label="Generated in conversations">
          <h3 class="font-medium">Generated in conversations</h3>
          <p class="text-muted-foreground text-sm">
            Each serves its conversation and lasts until Kelpy stops; kept in the vault, it serves every conversation.
          </p>
          <ul class="grid grid-cols-[minmax(0,1fr)] gap-1">
            {#each view.generated as secret (`${secret.session_id}:${secret.name}`)}
              <li class="flex flex-wrap items-center gap-x-2 gap-y-1 text-sm">
                <code class="min-w-0 truncate">{secret.name}</code>
                <span class="text-muted-foreground min-w-0 truncate">{title(secret.session_id)}</span>
                <span class="ml-auto flex shrink-0 gap-1">
                  <Button variant="ghost" size="sm" onclick={() => copy(secret.name, secret.session_id)}>
                    {copied === `${secret.session_id}:${secret.name}` ? 'Copied' : 'Copy'}
                  </Button>
                  {#if unlocked}
                    <Button
                      variant="ghost"
                      size="sm"
                      disabled={busy}
                      onclick={() => act(() => keepSecret(secret.name, secret.session_id))}
                    >
                      Keep in the vault
                    </Button>
                  {/if}
                </span>
              </li>
            {/each}
          </ul>
        </section>
      {/if}
    {:else if !error}
      <p class="text-muted-foreground text-sm">Loading…</p>
    {/if}
  </Dialog.Content>
</Dialog.Root>
