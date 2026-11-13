<script lang="ts" module>
  /** An image to show with the message, uploaded already. */
  export interface ComposerImage {
    /** Its name among the uploaded files. */
    name: string;
  }

  /** A message written in the box, to send. */
  export interface Outgoing {
    text: string;
    images: ComposerImage[];
  }

  /** The most images one message may show, as Kelpy takes them. */
  const MAX_IMAGES = 8;
</script>

<script lang="ts">
  import { onDestroy, onMount, tick } from 'svelte';
  import ArrowUpIcon from '@lucide/svelte/icons/arrow-up';
  import BrainIcon from '@lucide/svelte/icons/brain';
  import LightbulbIcon from '@lucide/svelte/icons/lightbulb';
  import PaperclipIcon from '@lucide/svelte/icons/paperclip';
  import ServerIcon from '@lucide/svelte/icons/server';
  import ShieldCheckIcon from '@lucide/svelte/icons/shield-check';
  import SquareIcon from '@lucide/svelte/icons/square';
  import XIcon from '@lucide/svelte/icons/x';
  import ModeMenu from './ModeMenu.svelte';
  import Thumbnail, { forgetThumbnail, seedThumbnail } from './Thumbnail.svelte';
  import { onFirstLine, onLastLine, PromptHistory } from './history';
  import { describe, uploadFile, type Access, type Approval, type MemoryUse } from './api';
  import { usable } from './connection.svelte';
  import { runtime } from './runtime.svelte';
  import { canRestart, elevation, restartAsAdministrator, switchMemory } from './elevation.svelte';
  import { ACCESS_OPTIONS, APPROVAL_OPTIONS } from './modes';
  import { isImage, pastedName } from './images';

  interface Props {
    /** The conversation the message is written in, none for a new one: an upload ending once the box shows another
     * conversation's message is left out of it, in Files. */
    conversation?: number;
    value: string;
    /** The images to show with the message, uploaded already. */
    images: ComposerImage[];
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
    conversation,
    value = $bindable(),
    images = $bindable(),
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
  /** The uploads under way: the conversation each is for, and whether it is an image, holding a place among them. */
  let uploading = $state.raw<{ owner: number | undefined; image: boolean }[]>([]);
  /** Uploads for this message are under way: it waits for them. */
  const busy = $derived(uploading.some((upload) => upload.owner === conversation));
  let gone = false;
  onDestroy(() => (gone = true));
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

  /**
   * Uploads `files`. Images go with the message, shown to the model and read by OCR; the paths of other files are
   * written in the message, for the model to read them there. A pasted screenshot is named after the moment.
   */
  async function attach(files: Iterable<File>) {
    // Taken now: the picker's list is emptied, and a drop's no longer readable, once the first upload waits.
    const chosen = [...files];
    uploadError = null;
    // The message the files go with: another conversation's, once the uploads end, is left alone.
    const owner = conversation;
    const current = () => !gone && conversation === owner;
    const full = `A message can show at most ${MAX_IMAGES} images.`;
    for (const file of chosen) {
      const image = isImage(file);
      // The images uploading for this message hold their places, so uploads at once cannot go past the limit.
      const held = uploading.filter((upload) => upload.image && upload.owner === owner).length;
      if (image && current() && images.length + held >= MAX_IMAGES) {
        uploadError = full;
        break;
      }
      const upload = { owner, image };
      uploading = [...uploading, upload];
      try {
        const named = image ? new File([file], pastedName(file, new Date()), { type: file.type }) : file;
        const uploaded = await uploadFile(named);
        if (!current()) {
          continue;
        }
        if (image && images.length >= MAX_IMAGES) {
          // Filled meanwhile, such as by an unsent message put back: the image stays in Files.
          uploadError = full;
        } else if (image) {
          seedThumbnail(uploaded.name, file);
          images = [...images, { name: uploaded.name }];
        } else {
          const separator = value === '' || value.endsWith('\n') ? '' : '\n';
          // In backticks: a path may hold spaces.
          value = `${value}${separator}\`${uploaded.path}\`\n`;
        }
      } catch (reason: unknown) {
        if (current()) {
          uploadError = `${file.name}: ${describe(reason)}`;
        }
      } finally {
        uploading = uploading.filter((other) => other !== upload);
      }
    }
    if (current()) {
      focus();
    }
  }

  // Another conversation's message: an upload's problem was this one's.
  $effect(() => {
    void conversation;
    uploadError = null;
  });

  /** Files pasted in the message box, such as a screenshot, are uploaded as dropped ones are; text pastes as text. */
  function onPaste(event: ClipboardEvent) {
    const files = [...(event.clipboardData?.files ?? [])];
    if (files.length > 0) {
      event.preventDefault();
      void attach(files);
    }
  }

  /** Whether there is something to send, its uploads done. */
  const sendable = $derived((value.trim() !== '' || images.length > 0) && !busy);

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
      if (!running && usable() && sendable) {
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
  /** The memories of the server Kelpy can use. The shared one needs administrator rights: chosen from here, Kelpy
   * restarts with them through the system's prompt, where it can. */
  const memoryOptions = $derived.by(() => {
    const view = elevation.view;
    const elevated = view?.elevated ?? false;
    const reachable = elevated || canRestart();
    const shared = elevation.memory?.shared;
    const where = shared ? `, in ${shared}` : '';
    const rights = elevated
      ? ''
      : reachable
        ? 'Kelpy restarts as administrator.'
        : view?.how === 'command'
          ? 'Not available: start Kelpy with sudo kelpy.'
          : 'Not available: start Kelpy as administrator on this computer.';
    return [
      { value: 'private' as MemoryUse, name: 'Private', description: 'Only for you, in your Kelpy home.' },
      {
        value: 'shared' as MemoryUse,
        name: 'Shared',
        description: `With the other administrators of this computer${where}. ${rights}`.trim(),
        disabled: !reachable,
      },
    ];
  });
  /** Under the Memory menu: why the memory in use is not the one asked for, if so, then who can use the shared one
   * and what changing it does. */
  const memoryNote = $derived.by(() => {
    const memory = elevation.memory;
    const scope =
      'Shared is available to administrators only, for security reasons. The memory applies to every conversation. ' +
      'Changing it restarts Kelpy in a few seconds, which stops the answers being written; the notes stay where they ' +
      'are, not copied to the other memory.';
    if (!memory?.problem) {
      return scope;
    }
    const problem = memory.problem.replace(/\.$/, '');
    const why =
      memory.chosen === 'shared' && memory.use === 'private'
        ? `The shared memory is not in use: ${problem.charAt(0).toLowerCase()}${problem.slice(1)}.`
        : `${problem.charAt(0).toUpperCase()}${problem.slice(1)}.`;
    return `${why} ${scope}`;
  });
  /** While the shared memory chosen cannot be opened, the private one in use can be chosen from now on: the menu's
   * Private, selected already, cannot be chosen again. */
  const keepPrivate = $derived(
    elevation.memory?.chosen === 'shared' && elevation.memory.use === 'private'
      ? { label: 'Use Private from now on', icon: BrainIcon, onclick: () => void switchMemory('private') }
      : undefined,
  );
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
  {#if images.length > 0}
    <div class="mb-2 flex flex-wrap gap-2">
      {#each images as image, index (image.name)}
        <div class="relative">
          <Thumbnail name={image.name}>
            {#snippet children(url)}
              <img src={url} alt={image.name} title={image.name} class="size-16 rounded-xl border object-cover" />
            {/snippet}
            {#snippet missing()}
              <span class="bg-muted inline-flex size-16 items-center justify-center rounded-xl border text-center text-[10px]">
                {image.name}
              </span>
            {/snippet}
          </Thumbnail>
          <button
            type="button"
            onclick={() => {
              images = images.filter((_, other) => other !== index);
              forgetThumbnail(image.name);
            }}
            aria-label="Remove {image.name}"
            title="Remove from the message: the file stays in Files"
            class="bg-foreground text-background absolute -top-1.5 -right-1.5 inline-flex size-5 items-center justify-center rounded-full"
          >
            <XIcon class="size-3" />
          </button>
        </div>
      {/each}
    </div>
  {/if}
  <textarea
    bind:this={field}
    bind:value
    onkeydown={onKeydown}
    onpaste={onPaste}
    oninput={() => recall.reset()}
    rows="1"
    placeholder="Write a message"
    aria-label="Message"
    class="placeholder:text-muted-foreground block field-sizing-content max-h-48 min-h-7 w-full resize-none bg-transparent py-0.5 leading-relaxed outline-none"
  ></textarea>
  {#if uploadError}
    <p class="text-destructive mt-1 text-xs" role="alert">{uploadError}</p>
  {/if}
  <!-- Send and Stop keep their place on the right; on a narrow screen the buttons and menus before them wrap. -->
  <div class="mt-2 flex items-end gap-2">
    <div class="flex min-w-0 flex-1 flex-wrap items-center gap-2">
      <button
        type="button"
        onclick={() => picker?.click()}
        disabled={busy || !usable()}
        aria-label="Attach files"
        title={busy
          ? 'Uploading…'
          : 'Upload files to Kelpy: images go with the message, other files as their paths. Files in the top bar lists them. A screenshot can also be pasted.'}
        class="text-muted-foreground hover:bg-muted hover:text-foreground inline-flex size-8 items-center justify-center rounded-full disabled:opacity-40"
      >
        <PaperclipIcon class="size-4 {busy ? 'animate-pulse' : ''}" />
      </button>
      <input
        bind:this={picker}
        type="file"
        multiple
        class="hidden"
        onchange={(event) => {
          void attach(event.currentTarget.files ?? []);
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
      <div class="ml-auto flex flex-wrap items-center justify-end gap-2">
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
        {#if elevation.memory}
          <ModeMenu
            label="Memory"
            title="Which memory of this server Kelpy reads and writes. Applies to every conversation."
            icon={BrainIcon}
            value={elevation.memory.use}
            options={memoryOptions}
            note={memoryNote}
            noteAction={keepPrivate}
            onChange={(use) => void switchMemory(use)}
          />
        {/if}
      </div>
    </div>
    {#if running}
      <button
        type="button"
        onclick={onStop}
        aria-label="Stop the answer"
        title="Stop"
        class="bg-foreground text-background inline-flex size-9 shrink-0 items-center justify-center rounded-full"
      >
        <SquareIcon class="size-3.5 fill-current" />
      </button>
    {:else}
      <button
        type="button"
        onclick={onSend}
        disabled={!sendable || !usable()}
        aria-label="Send"
        title={!usable() ? 'Kelpy cannot be used from this page now' : busy ? 'Wait for the files to upload' : 'Send'}
        class="bg-foreground text-background inline-flex size-9 shrink-0 items-center justify-center rounded-full transition-opacity disabled:opacity-30"
      >
        <ArrowUpIcon class="size-[18px]" />
      </button>
    {/if}
  </div>
</div>
