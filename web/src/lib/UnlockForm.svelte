<script lang="ts">
  import { Button } from '$lib/components/ui/button';
  import { Input } from '$lib/components/ui/input';
  import { Label } from '$lib/components/ui/label';
  import type { AgentKey, UnlockRequest } from './api';

  interface Props {
    /** The label of the button that submits it. */
    action: string;
    agentKeys: AgentKey[];
    agentProblem: string | null;
    busy: boolean;
    onSubmit: (unlock: UnlockRequest) => void;
    onCancel: () => void;
  }

  let { action, agentKeys, agentProblem, busy, onSubmit, onCancel }: Props = $props();

  const MIN_PASSPHRASE = 12;
  /** Letters and digits that cannot be mistaken for one another: 32 of them, 5 bits each. */
  const ALPHABET = 'ABCDEFGHJKLMNPQRSTUVWXYZ23456789';

  let method = $state<'passphrase' | 'ssh_key'>('passphrase');
  let passphrase = $state('');
  let shown = $state(false);
  let saved = $state(false);
  const usable = $derived(agentKeys.filter((key) => key.usable));
  let fingerprint = $state<string | null>(null);
  const chosen = $derived(fingerprint ?? usable[0]?.fingerprint ?? null);

  const ready = $derived(
    method === 'passphrase' ? [...passphrase].length >= MIN_PASSPHRASE && saved : chosen !== null,
  );

  /** Five groups of five characters: 125 bits. */
  function generate() {
    const bytes = crypto.getRandomValues(new Uint8Array(25));
    const characters = [...bytes].map((byte) => ALPHABET[byte % ALPHABET.length]);
    passphrase = [0, 5, 10, 15, 20].map((at) => characters.slice(at, at + 5).join('')).join('-');
    shown = true;
  }

  function submit(event: SubmitEvent) {
    event.preventDefault();
    if (!ready) {
      return;
    }
    onSubmit(method === 'passphrase' ? { method, passphrase } : { method, fingerprint: chosen });
  }
</script>

<form class="bg-muted/40 grid grid-cols-[minmax(0,1fr)] gap-3 rounded-lg border p-3" onsubmit={submit}>
  <div class="flex gap-4 text-sm" role="radiogroup" aria-label="Unlock with">
    <label class="flex items-center gap-2">
      <input type="radio" bind:group={method} value="passphrase" /> Passphrase
    </label>
    <label class="flex items-center gap-2">
      <input type="radio" bind:group={method} value="ssh_key" /> SSH key
    </label>
  </div>
  {#if method === 'passphrase'}
    <div class="grid grid-cols-[minmax(0,1fr)] gap-1.5">
      <Label for="vault-passphrase">Passphrase (at least {MIN_PASSPHRASE} characters)</Label>
      <div class="flex gap-2">
        <Input
          id="vault-passphrase"
          type={shown ? 'text' : 'password'}
          bind:value={passphrase}
          autocomplete="new-password"
          class="font-mono"
        />
        <Button type="button" variant="outline" onclick={generate}>Generate</Button>
        <Button type="button" variant="ghost" onclick={() => (shown = !shown)}>{shown ? 'Hide' : 'Show'}</Button>
      </div>
    </div>
    <label class="flex items-start gap-2 text-sm">
      <input type="checkbox" bind:checked={saved} class="mt-1" />
      <span>
        I have saved the passphrase. Without it, or another way to unlock the vault, its secrets cannot be recovered.
      </span>
    </label>
  {:else if usable.length > 0}
    <div class="grid grid-cols-[minmax(0,1fr)] gap-1.5">
      <Label for="vault-key">Key of the SSH agent of Kelpy's session</Label>
      <select
        id="vault-key"
        class="bg-background h-9 w-full min-w-0 rounded-md border px-2 text-sm"
        value={chosen}
        onchange={(event) => (fingerprint = event.currentTarget.value)}
      >
        {#each usable as key (key.fingerprint)}
          <option value={key.fingerprint}>{key.label}</option>
        {/each}
      </select>
      <p class="text-muted-foreground text-xs">
        The vault opens whenever Kelpy starts from a session whose agent holds this key, such as <code>ssh -A</code> from your
        computer.
      </p>
    </div>
  {:else}
    <p class="text-muted-foreground text-sm">
      {agentProblem ?? "The SSH agent of Kelpy's session holds no Ed25519 or RSA key: ECDSA and security keys sign differently each time."}
    </p>
  {/if}
  <div class="flex justify-end gap-2">
    <Button type="button" variant="ghost" onclick={onCancel}>Cancel</Button>
    <Button type="submit" disabled={!ready || busy}>{action}</Button>
  </div>
</form>
