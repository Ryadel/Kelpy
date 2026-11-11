<script lang="ts">
  import { onMount, tick } from 'svelte';
  import ArrowUpIcon from '@lucide/svelte/icons/arrow-up';
  import LightbulbIcon from '@lucide/svelte/icons/lightbulb';
  import PaperclipIcon from '@lucide/svelte/icons/paperclip';
  import ServerIcon from '@lucide/svelte/icons/server';
  import ShieldCheckIcon from '@lucide/svelte/icons/shield-check';
  import SquareIcon from '@lucide/svelte/icons/square';
  import ModeMenu from './ModeMenu.svelte';
  import { onFirstLine, onLastLine, PromptHistory } from './history';
  import { describe, uploadFile, type Access, type Approval } from './api';
  import { usable } from './connection.svelte';
  import { runtime } from './runtime.svelte';
  import { canRestart, restartAsAdministrator } from './elevation.svelte';
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
    /** The messages sent in this conversation, oldest first: Up and Down bring them back. */
    history?: readonly string[];
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
    history = [],
  }: Props = $props();

  let field = $state<HTMLTextAreaElement | null>(null);
  let picker = $state<HTMLInputElement | null>(null);
  let uploading = $state(0);
  let dragging = $state(false);
  let uploadError = $state<string | null>(null);
  const recall = new PromptHistory(() => history);
  // Another conversation, or a message sent: browsing starts again from the latest. Answers being written change
  // the messages, not what was sent, so they leave it alone.
  let sentKey = '';
  $effect(() => {
    const key = history.join('\u0000');
    if (key !== sentKey) {
      sentKey = key;
      recall.reset();
    }
  });

  /** Uploads `files` and writes their paths in the message, for the model to read them there. */
  async function attach(files: FileList | null) {
    uploadError = null;
    for (const file of files ?? []) {
      uploading += 1;
      try {
        const uploaded = await uploadFile(file);
        const separator = value === '' || value.endsWith('\n') ? '' : '\n';
        // In backticks: a path may hold spaces.
        value = `${value}${separator}\`${uploaded.path}\`\n`;
      } catch (reason: unknown) {
        uploadError = `${file.name}: ${describe(reason)}`;
      } finally {
        uploading -= 1;
      }
    }
    focus();
  }

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
    } else if ((event.key === 'ArrowUp' || event.key === 'ArrowDown') && field) {
      browse(event, field);
    }
  }

  /** Up on the box's first line brings back the message sent before; Down on its last line, the next one. */
  function browse(event: KeyboardEvent, box: HTMLTextAreaElement) {
    const plain = !(event.shiftKey || event.altKey || event.ctrlKey || event.metaKey || event.isComposing);
    const at = box.selectionStart;
    if (!plain || at !== box.selectionEnd) {
      return;
    }
    const up = event.key === 'ArrowUp';
    if (up ? !onFirstLine(value, at) : !onLastLine(value, at)) {
      return;
    }
    const recalled = up ? recall.back(value) : recall.forward();
    if (recalled === null) {
      return;
    }
    event.preventDefault();
    value = recalled;
    // Then at its end, where typing goes on.
    void tick().then(() => box.setSelectionRange(recalled.length, recalled.length));
  }
  /** Who Kelpy's actions run as, and what that rules out. */
  const account = $derived.by(() => {
    const status = runtime.status;
    return status ? `Kelpy runs as ${status.account}.${status.account_limits ? ` ${status.account_limits}` : ''}` : undefined;
  });
</script>

<!-- Files dropped on the message box are uploaded, and their paths written in the message. -->
<div
  role="group"
  class="bg-composer focus-within:border-primary/45 rounded-[28px] border px-4 pt-3 pb-2.5 shadow-sm {dragging
    ? 'border-primary'
    : ''}"
  ondragover={(event) => {
    if (event.dataTransfer?.types.includes('Files')) {
      event.preventDefault();
      dragging = true;
    }
  }}
  ondragleave={() => (dragging = false)}
  ondrop={(event) => {
    if (event.dataTransfer?.files.length) {
      event.preventDefault();
      dragging = false;
      void attach(event.dataTransfer.files);
    }
  }}
>
  <textarea
    bind:this={field}
    bind:value
    onkeydown={onKeydown}
    oninput={() => recall.reset()}
    rows="1"
    placeholder="Write a message"
    aria-label="Message"
    class="placeholder:text-muted-foreground block field-sizing-content max-h-48 min-h-7 w-full resize-none bg-transparent py-0.5 leading-relaxed outline-none"
  ></textarea>
  {#if uploadError}
    <p class="text-destructive mt-1 text-xs" role="alert">{uploadError}</p>
  {/if}
  <div class="mt-2 flex flex-wrap items-center gap-2">
    <button
      type="button"
      onclick={() => picker?.click()}
      disabled={uploading > 0 || !usable()}
      aria-label="Attach files"
      title={uploading > 0 ? 'Uploading…' : 'Upload files to Kelpy and write their paths in the message. Files in the top bar lists them.'}
      class="text-muted-foreground hover:bg-muted hover:text-foreground inline-flex size-8 items-center justify-center rounded-full disabled:opacity-40"
    >
      <PaperclipIcon class="size-4 {uploading > 0 ? 'animate-pulse' : ''}" />
    </button>
    <input
      bind:this={picker}
      type="file"
      multiple
      class="hidden"
      onchange={(event) => {
        void attach(event.currentTarget.files);
        event.currentTarget.value = '';
      }}
    />
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
        noteAction={canRestart()
          ? { label: 'Restart as administrator', icon: ShieldCheckIcon, onclick: () => void restartAsAdministrator() }
          : undefined}
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
