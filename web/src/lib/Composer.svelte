<script lang="ts">
  import { onMount } from 'svelte';
  import ArrowUpIcon from '@lucide/svelte/icons/arrow-up';
  import LightbulbIcon from '@lucide/svelte/icons/lightbulb';
  import ServerIcon from '@lucide/svelte/icons/server';
  import ShieldCheckIcon from '@lucide/svelte/icons/shield-check';
  import SquareIcon from '@lucide/svelte/icons/square';
  import ModeMenu from './ModeMenu.svelte';
  import type { Access, Approval } from './api';
  import { usable } from './connection.svelte';
  import { runtime } from './runtime.svelte';
  import { ACCESS_OPTIONS, APPROVAL_OPTIONS } from './modes';

  interface Props {
    value: string;
    /** Answers in this conversation explain each step. */
    explanations: boolean;
    onExplanations: (on: boolean) => void;
    access: Access;
    onAccess: (access: Access) => void;
    approval: Approval;
    onApproval: (approval: Approval) => void;
    /** An answer is being written: the send button becomes a stop button. */
    running?: boolean;
    onSend: () => void;
    onStop?: () => void;
  }

  let {
    value = $bindable(),
    explanations,
    onExplanations,
    access,
    onAccess,
    approval,
    onApproval,
    running = false,
    onSend,
    onStop,
  }: Props = $props();

  let field = $state<HTMLTextAreaElement | null>(null);

  /** Moves the cursor to the message box, such as after a suggestion filled it. */
  export function focus() {
    field?.focus();
  }

  onMount(() => {
    // With a mouse the box is ready for typing; on a touch screen focusing it would open the keyboard unasked.
    if (matchMedia('(pointer: fine)').matches) {
      focus();
    }
  });

  function onKeydown(event: KeyboardEvent) {
    if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      if (!running && usable()) {
        onSend();
      }
    }
  }
  /** Who Kelpy's actions run as, and what that rules out. */
  const account = $derived.by(() => {
    const status = runtime.status;
    return status ? `Kelpy runs as ${status.account}.${status.account_limits ? ` ${status.account_limits}` : ''}` : undefined;
  });
</script>

<div class="bg-composer focus-within:border-primary/45 rounded-[28px] border px-4 pt-3 pb-2.5 shadow-sm">
  <textarea
    bind:this={field}
    bind:value
    onkeydown={onKeydown}
    rows="1"
    placeholder="Write a message"
    aria-label="Message"
    class="placeholder:text-muted-foreground block field-sizing-content max-h-48 min-h-7 w-full resize-none bg-transparent py-0.5 leading-relaxed outline-none"
  ></textarea>
  <div class="mt-2 flex flex-wrap items-center gap-2">
    <button
      type="button"
      aria-pressed={explanations}
      onclick={() => onExplanations(!explanations)}
      title="Answers explain each step in plain words: what, why, what to expect, how to check. Applies to this conversation."
      class="text-muted-foreground hover:bg-muted hover:text-foreground aria-pressed:border-primary/40 aria-pressed:bg-primary/15 aria-pressed:text-primary inline-flex items-center gap-1.5 rounded-full border px-3 py-1 text-[13px]"
    >
      <LightbulbIcon class="size-4" /> Explanations
    </button>
    <div class="ml-auto flex items-center gap-2">
      <ModeMenu
        label="System access"
        title="What Kelpy may do on this system. Applies to this conversation."
        icon={ServerIcon}
        value={access}
        options={ACCESS_OPTIONS}
        note={account}
        onChange={onAccess}
      />
      <ModeMenu
        label="Approval mode"
        title={access === 'none'
          ? 'Kelpy takes no actions with System access None.'
          : 'When Kelpy asks before acting. Applies to this conversation.'}
        icon={ShieldCheckIcon}
        value={approval}
        options={APPROVAL_OPTIONS}
        disabled={access === 'none'}
        onChange={onApproval}
      />
    </div>
    {#if running}
      <button
        type="button"
        onclick={onStop}
        aria-label="Stop the answer"
        title="Stop"
        class="bg-foreground text-background inline-flex size-9 items-center justify-center rounded-full"
      >
        <SquareIcon class="size-3.5 fill-current" />
      </button>
    {:else}
      <button
        type="button"
        onclick={onSend}
        disabled={value.trim() === '' || !usable()}
        aria-label="Send"
        title={usable() ? 'Send' : 'Kelpy cannot be used from this page now'}
        class="bg-foreground text-background inline-flex size-9 items-center justify-center rounded-full transition-opacity disabled:opacity-30"
      >
        <ArrowUpIcon class="size-[18px]" />
      </button>
    {/if}
  </div>
</div>
