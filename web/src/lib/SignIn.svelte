<script lang="ts">
  import { Button } from '$lib/components/ui/button';
  import * as Card from '$lib/components/ui/card';
  import { Input } from '$lib/components/ui/input';
  import { Label } from '$lib/components/ui/label';
  import { signIn } from './api';

  interface Props {
    /** Shown above the form, for example after signing out or when a link's code was not accepted. */
    notice?: string | null;
    onSignedIn: () => void;
  }

  let { notice = null, onSignedIn }: Props = $props();

  let code = $state('');
  let busy = $state(false);
  let error = $state<string | null>(null);

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    busy = true;
    error = null;
    try {
      const result = await signIn(code);
      if (result.ok) {
        code = '';
        onSignedIn();
      } else if (result.reason === 'wrong-code') {
        error = 'That code is not right. Check it in the terminal and try again.';
      } else if (result.reason === 'too-many-attempts') {
        error = `Too many wrong codes. Wait ${result.retryAfter} seconds, then try again.`;
      } else {
        error = `Could not sign in: ${result.message}.`;
      }
    } catch {
      error = 'Kelpy cannot be reached. If it was stopped, start it again on the server and use the new code it shows.';
    } finally {
      busy = false;
    }
  }
</script>

<Card.Root class="mx-auto mt-16 w-full max-w-md">
  <Card.Header>
    <Card.Title>Sign in</Card.Title>
    <Card.Description>
      Enter the access code shown in the terminal where Kelpy is running, next to <em>Code</em>. It changes every
      time Kelpy starts.
    </Card.Description>
  </Card.Header>
  <Card.Content>
    {#if notice}
      <p class="text-muted-foreground mb-4 text-sm">{notice}</p>
    {/if}
    <form class="grid gap-3" onsubmit={submit}>
      <Label for="access-code">Access code</Label>
      <div class="flex gap-2">
        <Input
          id="access-code"
          bind:value={code}
          placeholder="XXXX-XXXX"
          autocomplete="off"
          autocapitalize="characters"
          spellcheck="false"
          maxlength={16}
          required
          class="font-mono tracking-wider uppercase"
        />
        <Button type="submit" disabled={busy || code.trim() === ''}>Sign in</Button>
      </div>
      {#if error}
        <p class="text-destructive text-sm" role="alert">{error}</p>
      {/if}
    </form>
  </Card.Content>
</Card.Root>
