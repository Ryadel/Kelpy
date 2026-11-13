<script lang="ts">
  import { tick, untrack } from 'svelte';
  import ArrowDownIcon from '@lucide/svelte/icons/arrow-down';
  import CheckIcon from '@lucide/svelte/icons/check';
  import CopyIcon from '@lucide/svelte/icons/copy';
  import * as Alert from '$lib/components/ui/alert';
  import { Badge } from '$lib/components/ui/badge';
  import { Button } from '$lib/components/ui/button';
  import ActionCard from './ActionCard.svelte';
  import CommandLog from './CommandLog.svelte';
  import Composer from './Composer.svelte';
  import Differences from './Differences.svelte';
  import Markdown from './Markdown.svelte';
  import MessageImages from './MessageImages.svelte';
  import PlanCard from './PlanCard.svelte';
  import Waiting from './Waiting.svelte';
  import Welcome from './Welcome.svelte';
  import { connection, usable } from './connection.svelte';
  import { loadDraft, saveDraft } from './drafts';
  import { commandLog, mergeOperations, pending, segments, stepStates } from './actions';
  import { copyText } from './clipboard';
  import { hasCallouts } from './explanations';
  import { sentMessages } from './history';
  import { splitImages } from './images';
  import type { ComposerImage, Outgoing } from './Composer.svelte';
  import { advance, settled } from './reveal';
  import {
    ApiError,
    cancelAnswer,
    decideAction,
    stopAction,
    describe,
    fetchConversation,
    fetchDifferences,
    followAnswer,
    sendMessage,
    setModes,
    Unreachable,
    type Action,
    type AnswerEvent,
    type Approval,
    type ChatEvent,
    type Difference,
    type Message,
    type Modes,
    type Session,
  } from './api';

  interface Props {
    session: Session;
    /** A first message, written before the conversation existed: it is sent as soon as the conversation opens. */
    start: Outgoing | null;
    onStarted: () => void;
    /** Called with the conversation as changed here, such as its explanations. */
    onChanged: (session: Session) => void;
    onOpenProvider: () => void;
    /** The Command log panel is open. */
    log: boolean;
    onCloseLog: () => void;
  }

  let { session, start, onStarted, onChanged, onOpenProvider, log, onCloseLog }: Props = $props();

  let messages = $state<Message[]>([]);
  /** The operations the administrator asked for in this conversation, such as putting a file back. */
  let administrator = $state<Action[]>([]);
  /** What Kelpy changed on the server and was changed outside it since, not settled yet. */
  let differences = $state<Difference[]>([]);
  /** The last message summarised for the model, if the older messages are. */
  let summarisedThrough = $state<number | null>(null);
  let loaded = $state(false);
  /** An answer is being written, by this page or by an earlier one. */
  let running = $state(false);
  let draft = $state('');
  /** Follow the conversation, and are set at once when the user changes them. */
  let explanations = $derived(session.explanations);
  let access = $derived(session.access);
  let approval = $derived(session.approval);
  let error = $state<{ text: string; provider: boolean } | null>(null);
  /** Why answers ended early, as reported while this page watched them. */
  let endings = $state<Record<number, string>>({});
  let scroller = $state<HTMLElement | null>(null);
  /** The messages inside the scroller: their growth is followed. */
  let content = $state<HTMLElement | null>(null);
  /**
   * The view keeps the end of the conversation in sight as it grows, until the user scrolls up; back at the end, or
   * sending a message, it follows again.
   */
  let following = $state(true);
  /** The answer whose text is being revealed, and how many of its characters arrived are shown. */
  let reveal = $state<{ id: number; shown: number } | null>(null);
  /** The answers whose explanation panels are closed. */
  let closedExplanations = $state<Record<number, boolean>>({});
  /** The message just copied, for its button to say so. */
  let copied = $state<number | null>(null);
  /** The animation frame revealing it, and when the last one ran. */
  let frame = 0;
  let frameAt = 0;
  let composer = $state<Composer | null>(null);
  let listening: AbortController | null = null;
  /** Counts the loads, so that only the latest one shows. */
  let loads = 0;

  /** The images to show with the next message, uploaded already. */
  let images = $state<ComposerImage[]>([]);

  /** What Up and Down bring back in the message box: this conversation's messages, as written. */
  const sent = $derived(
    sentMessages(messages.map((message) => ({ role: message.role, content: splitImages(message.content).text }))),
  );

  /** Before its first message a conversation shows the welcome, with the message box in the middle. */
  const empty = $derived(loaded && messages.length === 0 && !running);

  /** Every action of the conversation, the answers' and the administrator's, in order: the Command log. */
  const actions = $derived(commandLog(messages, administrator));
  /** An action waits for the administrator's decision. */
  const waitingApproval = $derived(actions.some((action) => action.state === 'waiting'));

  /** The first message the model reads as it is, after the summarised ones: the marker goes above it. */
  const firstUnsummarised = $derived(
    summarisedThrough === null ? null : (messages.find((message) => message.id > summarisedThrough!)?.id ?? null),
  );

  const statusLabels: Partial<Record<Message['status'], string>> = {
    interrupted: 'Incomplete',
    failed: 'Failed',
    refused: 'Declined',
  };

  /**
   * Reloading the list gives every conversation a new object: the conversation is reloaded only when its id changes,
   * or each reload would restart the answer being followed.
   */
  const sessionId = $derived(session.id);

  $effect(() => {
    const id = sessionId;
    messages = [];
    administrator = [];
    summarisedThrough = null;
    endings = {};
    error = null;
    loaded = false;
    stopRevealing();
    draft = untrack(() => loadDraft(id));
    images = [];
    void load(id);
    return () => {
      listening?.abort();
      stopRevealing();
    };
  });

  // Declared after the effect above, which puts each conversation's own text in the box first.
  $effect(() => saveDraft(sessionId, draft));

  // Back after missing some changes: an answer may have started or ended meanwhile. One followed here reports
  // its end by itself.
  $effect(() => {
    if (connection.resumed > 0) {
      untrack(() => !listening && void load(session.id));
    }
  });

  /** An error to show above the message box; the page already says when Kelpy cannot be reached. */
  function report(reason: unknown, provider = false) {
    error = reason instanceof Unreachable ? null : { text: describe(reason), provider };
  }

  async function load(id: number) {
    const number = ++loads;
    try {
      const conversation = await fetchConversation(id);
      if (id !== session.id || number !== loads) {
        return;
      }
      messages = conversation.messages;
      administrator = mergeOperations(conversation.administrator_actions, administrator);
      summarisedThrough = conversation.summarised_through;
      running = conversation.running;
      loaded = true;
      void refreshDifferences();
      if (start !== null && messages.length === 0 && !running) {
        // Sent in the same step as `loaded`, so the welcome never shows in between.
        draft = start.text;
        images = start.images;
        onStarted();
        await send();
        return;
      }
      await scrollToEnd(true);
      // An answer started by another page or before a reload: follow it until it ends.
      if (running && !listening) {
        await follow(id);
      }
    } catch (reason: unknown) {
      report(reason);
    }
  }

  /** Follows the answer being written, started elsewhere. */
  async function follow(id: number) {
    if (!(await listen((signal) => followAnswer(id, apply, signal)))) {
      // It ended before following started: the conversation loaded holds its text so far, not its final one.
      const ended = await fetchConversation(id);
      if (id === session.id && !listening) {
        messages = ended.messages;
        summarisedThrough = ended.summarised_through;
      }
    }
  }

  /**
   * An answer started or ended in some conversation, told by Kelpy's events. One this page sends or follows reaches
   * it through its own stream; another one in this conversation, sent by another page or interface, is followed.
   */
  export function receive(event: AnswerEvent) {
    const id = session.id;
    if (event.session_id !== id || listening) {
      return;
    }
    if (!loaded) {
      // The load under way may have read the conversation before the change.
      void load(id);
      return;
    }
    if (event.type === 'answer_started') {
      keepMessage(event.user);
      keepMessage(event.answer);
      void scrollToEnd(true);
      follow(id).catch(report);
    } else {
      apply({ ...event, type: 'done' });
    }
  }

  /** An operation of the administrator in some conversation started or ended: one in this conversation is shown. */
  export function receiveAction(action: Action) {
    if (action.session_id !== session.id) {
      return;
    }
    const index = administrator.findIndex((other) => other.id === action.id);
    if (index >= 0) {
      administrator[index] = action;
    } else {
      administrator.push(action);
    }
  }

  /** Reads again the changes made outside Kelpy. */
  export async function refreshDifferences() {
    try {
      differences = await fetchDifferences();
    } catch {
      // The conversation stays usable; the next change or check reads them again.
    }
  }

  /** Shows a message as it is now, in its place or at the end. */
  function keepMessage(message: Message) {
    const index = messages.findIndex((other) => other.id === message.id);
    if (index >= 0) {
      messages[index] = message;
    } else {
      messages.push(message);
    }
  }

  /** Shows the end of the conversation if the view follows it; `always` makes it follow again. */
  async function scrollToEnd(always = false) {
    if (always) {
      following = true;
    }
    await tick();
    if (following && scroller) {
      scroller.scrollTop = scroller.scrollHeight;
    }
  }

  /** Whether the view is at the end, as the user left it: decided when they scroll, never after content grows. */
  function scrolled() {
    if (scroller) {
      following = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 40;
    }
  }

  // Whatever makes the conversation taller, or its view shorter: text, an action card, an output opened, the message
  // box growing.
  $effect(() => {
    if (!scroller || !content) {
      return;
    }
    const view = scroller;
    const observer = new ResizeObserver(() => {
      if (following) {
        view.scrollTop = view.scrollHeight;
      }
    });
    observer.observe(content);
    observer.observe(view);
    return () => observer.disconnect();
  });

  function apply(event: ChatEvent) {
    switch (event.type) {
      case 'user':
      case 'started':
        // A followed answer is already listed, with the text saved so far: the event brings it up to date.
        keepMessage(event.message);
        break;
      case 'delta': {
        const answer = messages.at(-1);
        if (answer?.role === 'assistant') {
          if (reveal?.id !== answer.id) {
            reveal = { id: answer.id, shown: answer.content.length };
          }
          answer.content += event.text;
          startRevealing();
        }
        break;
      }
      case 'action': {
        // The text before an action is whole: it shows before the action's card.
        stopRevealing();
        const actions = messages.find((message) => message.id === event.action.message_id)?.actions;
        const index = actions?.findIndex((action) => action.id === event.action.id) ?? -1;
        if (index >= 0) {
          actions![index] = event.action;
        } else {
          actions?.push(event.action);
        }
        break;
      }
      case 'done': {
        stopRevealing();
        const index = messages.findIndex((message) => message.id === event.message.id);
        if (index >= 0) {
          messages[index] = event.message;
        }
        summarisedThrough = event.summarised_through;
        if (event.error && event.message.status !== 'complete') {
          endings[event.message.id] = event.error;
        }
        break;
      }
    }
    void scrollToEnd(event.type === 'user');
  }

  async function send() {
    const content = draft.trim();
    const shown = images;
    if ((!content && shown.length === 0) || running) {
      return;
    }
    error = null;
    draft = '';
    images = [];
    const id = session.id;
    // Once Kelpy has stored the message, it is in the conversation: giving it back to the box would send it twice.
    let stored = false;
    try {
      await listen((signal) =>
        sendMessage(
          id,
          content,
          shown.map((image) => image.name),
          (event) => {
            stored ||= event.type === 'user';
            apply(event);
          },
          signal,
        ),
      );
    } catch (reason: unknown) {
      if (!stored && id === session.id) {
        draft = content;
        images = shown;
      } else if (!stored) {
        saveDraft(id, content);
      }
      // Stopped before it was sent, such as while its images were read: back in the box, nothing to report.
      if (reason instanceof ApiError && reason.code === 'stopped') {
        return;
      }
      report(reason, reason instanceof ApiError && ['not_configured', 'missing_key', 'not_signed_in'].includes(reason.code));
    }
  }

  /**
   * Runs `stream`, the events of one answer, while this page shows the conversation. Leaving the conversation aborts
   * it: the answer goes on, on the server. Returns whether the stream ran to the answer's end.
   */
  async function listen(stream: (signal: AbortSignal) => Promise<void>): Promise<boolean> {
    const controller = new AbortController();
    listening = controller;
    running = true;
    try {
      await stream(controller.signal);
      return true;
    } catch (reason: unknown) {
      // Leaving the conversation, or the answer ending just before following started, are not errors.
      if (controller.signal.aborted || (reason instanceof ApiError && reason.code === 'not_running')) {
        return false;
      }
      throw reason;
    } finally {
      if (listening === controller) {
        listening = null;
        running = false;
      }
    }
  }

  async function stop() {
    try {
      await cancelAnswer(session.id);
    } catch (reason: unknown) {
      report(reason);
    }
  }

  /** Changes a mode of the conversation, shown at once and put back if Kelpy refuses. */
  async function change(modes: Partial<Modes>, show: (modes: Modes) => void) {
    const before: Modes = { explanations, access, approval };
    show({ ...before, ...modes });
    try {
      onChanged(await setModes(session.id, modes));
    } catch (reason: unknown) {
      show(before);
      report(reason);
    }
  }

  const changeExplanations = (on: boolean) => change({ explanations: on }, (modes) => (explanations = modes.explanations));
  const changeAccess = (chosen: Session['access']) => change({ access: chosen }, (modes) => (access = modes.access));
  const changeApproval = (chosen: Approval) => change({ approval: chosen }, (modes) => (approval = modes.approval));

  async function decide(action: Action, decision: 'approve' | 'deny', switchTo?: Approval) {
    try {
      await decideAction(session.id, action.id, decision, switchTo);
      if (switchTo) {
        approval = switchTo;
      }
    } catch (reason: unknown) {
      report(reason);
    }
  }

  /** Stops one action running; the answer goes on. */
  async function stopOne(action: Action) {
    try {
      await stopAction(session.id, action.id);
    } catch (reason: unknown) {
      report(reason);
    }
  }

  /** An answer still being written shows the waiting hint while nothing else moves: no text yet after its last
   * action, and no action under way. */
  function thinking(message: Message): boolean {
    if (message.status !== 'streaming' || message.actions.some(pending)) {
      return false;
    }
    return segments(shown(message)).at(-1)?.kind !== 'text';
  }

  /** Whether an answer holds explanations shown as callouts: those after a command are tabs of its code block. */
  const explained = (message: Message) => hasCallouts(message.content);

  /** Copies a message as written: an answer in Markdown. */
  async function copyMessage(message: Message) {
    if (await copyText(message.content)) {
      copied = message.id;
      setTimeout(() => {
        if (copied === message.id) {
          copied = null;
        }
      }, 2000);
    }
  }

  /** `message` as shown: an answer being revealed only with its text shown so far, settled. */
  function shown(message: Message): Message {
    if (reveal?.id !== message.id || message.status !== 'streaming') {
      return message;
    }
    return { ...message, content: settled(message.content.slice(0, reveal.shown)) };
  }

  /** Reveals the answer's text that arrived, a few words a frame, until all of it shows. */
  function startRevealing() {
    if (frame === 0) {
      frameAt = performance.now();
      frame = requestAnimationFrame(revealMore);
    }
  }

  function revealMore(now: number) {
    frame = 0;
    const answer = reveal && messages.find((message) => message.id === reveal!.id);
    if (!reveal || !answer) {
      return;
    }
    reveal.shown = advance(reveal.shown, answer.content, now - frameAt);
    frameAt = now;
    if (reveal.shown < answer.content.length) {
      frame = requestAnimationFrame(revealMore);
    }
  }

  /** Shows all the text arrived, at once. */
  function stopRevealing() {
    cancelAnimationFrame(frame);
    frame = 0;
    reveal = null;
  }

  function suggest(text: string) {
    draft = text;
    composer?.focus();
  }
</script>

{#snippet copyButton(message: Message, title: string, extra: string)}
  <button
    type="button"
    class="text-muted-foreground hover:bg-muted hover:text-foreground inline-flex items-center gap-1 rounded-md px-2 py-1 text-xs {extra}"
    {title}
    onclick={() => copyMessage(message)}
  >
    {#if copied === message.id}<CheckIcon class="size-3.5" /> Copied{:else}<CopyIcon class="size-3.5" /> Copy{/if}
  </button>
{/snippet}

{#snippet messageBox()}
  {#if error}
    <Alert.Root variant="destructive" class="mb-3">
      <Alert.Description>
        {error.text}
        {#if error.provider}
          <Button variant="link" class="h-auto p-0" onclick={onOpenProvider}>Set up the AI provider</Button>
        {/if}
      </Alert.Description>
    </Alert.Root>
  {/if}
  <Composer
    bind:this={composer}
    conversation={session.id}
    bind:value={draft}
    bind:images
    {explanations}
    onExplanations={changeExplanations}
    {access}
    onAccess={changeAccess}
    {approval}
    onApproval={changeApproval}
    {running}
    onSend={send}
    onStop={stop}
    history={sent}
  />
{/snippet}

<div class="flex min-h-0 flex-1">
  <div class="flex min-w-0 flex-1 flex-col">
    <Differences
      {differences}
      sessionId={session.id}
      onOperated={receiveAction}
      onChanged={(changed) => (differences = changed)}
    />
    {#if empty}
      <Welcome onSuggest={suggest}>{@render messageBox()}</Welcome>
    {:else}
      <div class="relative flex min-h-0 flex-1 flex-col">
      <div class="min-h-0 flex-1 overflow-y-auto" bind:this={scroller} onscroll={scrolled}>
        <div class="mx-auto flex max-w-conversation flex-col gap-8 px-5 pt-4 pb-10" bind:this={content}>
          {#each messages as message (message.id)}
            {#if message.id === firstUnsummarised}
              <div class="text-muted-foreground flex items-center gap-3 text-xs" role="separator">
                <span class="bg-border h-px flex-1"></span>
                Earlier messages summarised for the model
                <span class="bg-border h-px flex-1"></span>
              </div>
            {/if}
            {#if message.role === 'user'}
              {@const written = splitImages(message.content)}
              <div class="group ml-auto flex max-w-[78%] flex-col items-end gap-1">
                {#if written.images.length > 0}
                  <MessageImages images={written.images} />
                {/if}
                {#if written.text !== ''}
                  <div class="bg-bubble rounded-3xl px-5 py-2.5 whitespace-pre-wrap [overflow-wrap:anywhere]">
                    {written.text}
                  </div>
                {/if}
                {@render copyButton(message, 'Copies your message', 'opacity-0 group-hover:opacity-100 focus-visible:opacity-100')}
              </div>
            {:else}
              <div class="flex max-w-full flex-col gap-3">
                {#each segments(shown(message)) as segment, index (index)}
                  {#if segment.kind === 'text'}
                    <Markdown text={segment.text} explanationsOpen={!closedExplanations[message.id]} />
                  {:else}
                    {#each segment.actions as action (action.id)}
                      {#if action.plan}
                        <PlanCard
                          {action}
                          steps={stepStates(action, actions)}
                          canDecide={usable() && message.status === 'streaming'}
                          onDecide={(decision) => decide(action, decision)}
                        />
                      {:else}
                        <ActionCard
                          {action}
                          {approval}
                          {access}
                          canDecide={usable() && message.status === 'streaming'}
                          onDecide={(decision, switchTo) => decide(action, decision, switchTo)}
                          onStop={() => stopOne(action)}
                        />
                      {/if}
                    {/each}
                  {/if}
                {/each}
                {#if thinking(message)}
                  <Waiting />
                {/if}
                {#if message.status !== 'streaming' && message.content}
                  <div class="text-muted-foreground -mt-1 flex items-center gap-1 text-xs">
                    {@render copyButton(message, 'Copies the whole answer, in Markdown', '')}
                    {#if explained(message)}
                      <button
                        type="button"
                        class="hover:bg-muted hover:text-foreground rounded-md px-2 py-1"
                        onclick={() => (closedExplanations[message.id] = !closedExplanations[message.id])}
                      >
                        {closedExplanations[message.id] ? 'Open explanations' : 'Close explanations'}
                      </button>
                    {/if}
                  </div>
                {/if}
                {#if statusLabels[message.status]}
                  <div class="mt-2 flex items-center gap-2 text-sm">
                    <Badge variant={message.status === 'interrupted' ? 'secondary' : 'destructive'}>
                      {statusLabels[message.status]}
                    </Badge>
                    {#if endings[message.id]}
                      <span class="text-muted-foreground">{endings[message.id]}</span>
                    {/if}
                  </div>
                {/if}
              </div>
            {/if}
          {/each}
        </div>
      </div>
      {#if !following}
        <Button
          variant={waitingApproval ? 'default' : 'outline'}
          size="sm"
          class="absolute bottom-3 left-1/2 -translate-x-1/2 rounded-full shadow-md"
          onclick={() => scrollToEnd(true)}
          aria-label="Go to the end of the conversation"
          title="Go to the end of the conversation"
        >
          <ArrowDownIcon class="size-4" />
          {#if waitingApproval}Kelpy is waiting for your approval{:else if running}Kelpy is writing{/if}
        </Button>
      {/if}
      </div>
      <div class="mx-auto w-full max-w-conversation px-4 pb-3">
        {@render messageBox()}
        <p class="text-muted-foreground mt-2 text-center text-xs">Enter sends, Shift+Enter starts a new line.</p>
      </div>
    {/if}
  </div>
  {#if log}
    <CommandLog
      {actions}
      sessionId={session.id}
      title={session.title}
      onOperated={receiveAction}
      onClose={onCloseLog}
    />
  {/if}
</div>
