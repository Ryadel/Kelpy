<script lang="ts">
  import * as Alert from '$lib/components/ui/alert';
  import Composer from './Composer.svelte';
  import Welcome from './Welcome.svelte';
  import { createSession, describe, Unreachable, type Modes, type Session } from './api';
  import { loadDraft, saveDraft } from './drafts';
  import { DEFAULT_MODES } from './modes';

  interface Props {
    /** The conversation exists, with the modes chosen here: `message` is its first message, still to send. */
    onCreated: (session: Session, message: string) => void;
  }

  let { onCreated }: Props = $props();

  // The conversation is created with the first message, so starting one and leaving leaves nothing behind.
  let draft = $state(loadDraft('new'));
  let modes = $state<Modes>({ ...DEFAULT_MODES });
  let creating = false;
  let error = $state<string | null>(null);
  let composer = $state<Composer | null>(null);

  $effect(() => saveDraft('new', draft));

  async function send() {
    const message = draft.trim();
    if (!message || creating) {
      return;
    }
    creating = true;
    error = null;
    try {
      const session = await createSession(modes);
      // Cleared before the conversation opens: this component is gone by then.
      saveDraft('new', '');
      onCreated(session, message);
    } catch (reason: unknown) {
      // The page already says when Kelpy cannot be reached.
      error = reason instanceof Unreachable ? null : describe(reason);
    } finally {
      creating = false;
    }
  }
</script>

<Welcome
  onSuggest={(text) => {
    draft = text;
    composer?.focus();
  }}
>
  {#if error}
    <Alert.Root variant="destructive" class="mb-3">
      <Alert.Description>{error}</Alert.Description>
    </Alert.Root>
  {/if}
  <Composer
    bind:this={composer}
    bind:value={draft}
    explanations={modes.explanations}
    onExplanations={(on) => (modes.explanations = on)}
    access={modes.access}
    onAccess={(access) => (modes.access = access)}
    approval={modes.approval}
    onApproval={(approval) => (modes.approval = approval)}
    onSend={send}
  />
</Welcome>
