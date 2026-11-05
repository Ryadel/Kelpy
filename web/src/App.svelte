<script lang="ts">
  import { onMount, untrack } from 'svelte';
  import CheckIcon from '@lucide/svelte/icons/check';
  import ChevronDownIcon from '@lucide/svelte/icons/chevron-down';
  import CpuIcon from '@lucide/svelte/icons/cpu';
  import LogOutIcon from '@lucide/svelte/icons/log-out';
  import PanelLeftIcon from '@lucide/svelte/icons/panel-left';
  import PowerIcon from '@lucide/svelte/icons/power';
  import SlidersHorizontalIcon from '@lucide/svelte/icons/sliders-horizontal';
  import SquarePenIcon from '@lucide/svelte/icons/square-pen';
  import SquareTerminalIcon from '@lucide/svelte/icons/square-terminal';
  import { Button } from '$lib/components/ui/button';
  import * as Dialog from '$lib/components/ui/dialog';
  import * as DropdownMenu from '$lib/components/ui/dropdown-menu';
  import * as Alert from '$lib/components/ui/alert';
  import { cn } from '$lib/utils';
  import {
    describe,
    fetchProvider,
    finishChatGptSignIn,
    keepChatGptReturn,
    takeChatGptReturn,
    fetchSessions,
    fetchStatus,
    signIn,
    signOut,
    takeCodeFromLink,
    Unauthorized,
    Unreachable,
    type ChatGptCallback,
    type ProviderInfo,
    type RuntimeEvent,
    type RuntimeStatus,
    type Session,
  } from './lib/api';
  import Chat from './lib/Chat.svelte';
  import NewConversation from './lib/NewConversation.svelte';
  import ProviderDialog from './lib/ProviderDialog.svelte';
  import Sessions from './lib/Sessions.svelte';
  import SignIn from './lib/SignIn.svelte';
  import StopDialog from './lib/StopDialog.svelte';
  import ThemeToggle from './lib/ThemeToggle.svelte';
  import { connection, usable, watchConnection } from './lib/connection.svelte';
  import { kept, merged } from './lib/conversations';
  import { runtime } from './lib/runtime.svelte';
  import { watchEvents } from './lib/events';

  type View =
    | { kind: 'connecting' }
    | { kind: 'sign-in'; notice: string | null }
    | { kind: 'ready' }
    | { kind: 'stopped' }
    | { kind: 'error'; message: string };

  let view = $state<View>({ kind: 'connecting' });
  const status = $derived(runtime.status);
  let provider = $state<ProviderInfo | null>(null);
  let sessions = $state<Session[]>([]);
  /** The open conversation; none means a new one, created with its first message. */
  let selected = $state<number | null>(null);
  /** The first message of a conversation just created, for its `Chat` to send. */
  let start = $state<{ id: number; message: string } | null>(null);
  let providerOpen = $state(false);
  /** The outcome of a return from OpenAI's sign-in page, shown by the provider dialog. */
  let afterChatGpt = $state<{ error: string | null } | null>(null);
  let stopOpen = $state(false);
  /** On wide screens the conversations are a sidebar, which can be hidden. */
  let sidebarOpen = $state(true);
  /** On small screens the conversations open in a panel instead. */
  let sessionsOpen = $state(false);
  /** The Command log panel, beside the open conversation: it stays open, or closed, across reloads. */
  const LOG_KEY = 'kelpy.command-log';
  let logOpen = $state(localStorage.getItem(LOG_KEY) === 'open');
  $effect(() => localStorage.setItem(LOG_KEY, logOpen ? 'open' : 'closed'));
  let chat = $state<Chat | null>(null);
  /** Ends the subscription to Kelpy's events, which keeps the page in step with every other page and interface. */
  let stopEvents: (() => void) | null = null;

  const current = $derived(sessions.find((session) => session.id === selected) ?? null);

  /** The name of the configured service, such as `Groq`. */
  const serviceName = $derived.by(() => {
    const configured = provider?.configured;
    const name = provider?.services.find((service) => service.id === configured?.service)?.name;
    return configured ? (name ?? configured.kind) : null;
  });

  const needed = $derived(
    provider?.needs === 'sign_in' ? 'Not signed in' : provider?.needs === 'key' ? 'No API key' : null,
  );

  /**
   * A request failed. Without a session, a page opened by the user asks for the code; a page already in use keeps
   * what it shows and only says that its session ended (see `connection.expired`). When Kelpy cannot be reached, the
   * page stays as it is, marked as disconnected, and catches up once Kelpy answers again.
   */
  function fail(reason: unknown, notice: string | null = null) {
    if (reason instanceof Unauthorized) {
      if (view.kind === 'ready') {
        connection.expired = true;
      } else {
        view = { kind: 'sign-in', notice };
      }
    } else if (!(reason instanceof Unreachable)) {
      view = { kind: 'error', message: describe(reason) };
    }
  }

  // Kelpy answers again: a page still connecting loads. A page in use catches up when its events resume.
  let wasReachable = true;
  $effect(() => {
    const reachable = connection.reachable;
    if (reachable && !wasReachable) {
      untrack(() => view.kind === 'connecting' && void load());
    }
    wasReachable = reachable;
  });

  // Signed out, stopped or failed: nothing more to follow.
  $effect(() => {
    if (view.kind !== 'ready' && view.kind !== 'connecting') {
      untrack(endEvents);
    }
  });

  function endEvents() {
    stopEvents?.();
    stopEvents = null;
  }

  async function catchUp() {
    try {
      let fetched: Session[];
      [runtime.status, provider, fetched] = await Promise.all([fetchStatus(), fetchProvider(), fetchSessions()]);
      sessions = merged(sessions, fetched);
      connection.resumed += 1;
    } catch (reason: unknown) {
      fail(reason);
    }
  }

  async function load(notice: string | null = null) {
    endEvents();
    try {
      const events = watchEvents({ onEvent: receive, onResumed: () => void catchUp(), onEnded: fail });
      stopEvents = events.stop;
      await events.subscribed;
      let fetched: Session[];
      [runtime.status, provider, fetched] = await Promise.all([fetchStatus(), fetchProvider(), fetchSessions()]);
      sessions = merged(sessions, fetched);
      selected ??= sessions[0]?.id ?? null;
      view = { kind: 'ready' };
      const callback = takeChatGptReturn();
      if (callback) {
        await finishChatGpt(callback);
      } else if (!provider.configured || provider.needs) {
        // First visit, or a sign-in or key lost when Kelpy stopped: ask for it before the first message, not after.
        providerOpen = true;
      }
    } catch (reason: unknown) {
      fail(reason, notice);
    }
  }

  /** Completes a sign-in with ChatGPT, back from OpenAI's page, and shows the outcome in the provider dialog. */
  async function finishChatGpt(callback: ChatGptCallback) {
    let error: string | null = null;
    try {
      await finishChatGptSignIn(callback);
    } catch (reason: unknown) {
      if (reason instanceof Unauthorized) {
        throw reason;
      }
      error = describe(reason);
    }
    afterChatGpt = { error };
    providerOpen = true;
  }

  $effect(() => {
    if (!providerOpen) {
      afterChatGpt = null;
    }
  });

  async function refreshProvider() {
    try {
      provider = await fetchProvider();
    } catch (reason: unknown) {
      fail(reason);
    }
  }

  /** A change made here or anywhere else: by this page, another page, or another interface. */
  function receive(event: RuntimeEvent) {
    switch (event.type) {
      case 'session_created':
      case 'session_changed':
        keep(event.session);
        break;
      case 'session_deleted':
        forget(event.id);
        break;
      case 'answer_started':
      case 'answer_ended':
        chat?.receive(event);
        break;
      case 'action_changed':
        chat?.receiveAction(event.action);
        break;
      case 'differences_changed':
        void chat?.refreshDifferences();
        break;
      case 'provider_changed':
        void refreshProvider();
        break;
      case 'resync':
        void catchUp();
        break;
      case 'stopping':
        view = { kind: 'stopped' };
        break;
    }
  }

  /** Puts a conversation in the list as it is now, most recent first, unless the list has a newer copy. */
  function keep(session: Session) {
    sessions = kept(sessions, session);
  }

  function forget(id: number) {
    sessions = sessions.filter((session) => session.id !== id);
    if (selected === id) {
      selected = sessions[0]?.id ?? null;
    }
  }

  function select(id: number | null) {
    selected = id;
    sessionsOpen = false;
  }

  function started(session: Session, message: string) {
    keep(session);
    start = { id: session.id, message };
    select(session.id);
  }

  async function signOutAndForget() {
    try {
      await signOut();
      view = { kind: 'sign-in', notice: 'You are signed out.' };
    } catch (reason: unknown) {
      fail(reason);
    }
  }

  onMount(() => {
    if (keepChatGptReturn()) {
      return;
    }
    const stopWatching = watchConnection();
    // A link opened again in this tab only changes the fragment, which does not load the page: load it.
    const reloadOnLink = () => /^#code=/.test(window.location.hash) && window.location.reload();
    window.addEventListener('hashchange', reloadOnLink);
    const code = takeCodeFromLink();
    if (code === null) {
      void load();
    } else {
      signIn(code)
        .then((result) =>
          load(result.ok ? null : 'The code in the link was not accepted: Kelpy may have restarted since.'),
        )
        .catch((reason: unknown) => fail(reason));
    }
    return () => {
      stopWatching();
      endEvents();
      window.removeEventListener('hashchange', reloadOnLink);
    };
  });
</script>

{#snippet unreachable()}
  Kelpy cannot be reached. This page reconnects by itself when it can. If Kelpy was stopped, start it again on the
  server and open the new link it shows.
{/snippet}

{#snippet brand()}
  <h1 class="flex shrink-0 items-center gap-2.5 px-1.5 text-[17px] font-semibold tracking-tight">
    <!-- The black mark in both themes; on the dark one a faint turquoise glow lifts it from the background. -->
    <span class="relative inline-grid size-7 shrink-0 place-items-center">
      <span class="mark-glow -inset-3" aria-hidden="true"></span>
      <img src="/brand/kelpy-light.svg" alt="" width="28" height="28" class="relative size-7 object-contain" />
    </span>
    Kelpy
  </h1>
{/snippet}

{#if view.kind === 'ready'}
  <div class="flex h-dvh overflow-hidden">
    {#if sidebarOpen}
      <aside class="bg-sidebar text-sidebar-foreground hidden w-[260px] shrink-0 flex-col md:flex">
        <div class="flex h-14 shrink-0 items-center px-3">
          {@render brand()}
          <Button
            variant="ghost"
            size="icon-lg"
            class="text-muted-foreground ml-auto"
            onclick={() => (sidebarOpen = false)}
            aria-label="Hide the conversations"
            title="Hide the conversations"
          >
            <PanelLeftIcon class="size-[18px]" />
          </Button>
        </div>
        <div class="min-h-0 flex-1">
          <Sessions {sessions} {selected} onSelect={select} onCreate={() => select(null)} onRenamed={keep} onDeleted={forget} />
        </div>
        {#if status}
          <p class="text-muted-foreground truncate border-t px-4 py-2.5 text-xs" title={status.home}>
            v{status.version} · {status.home}
          </p>
        {/if}
      </aside>
    {/if}

    <main class="flex min-w-0 flex-1 flex-col">
      <header class="flex h-14 shrink-0 items-center gap-1 px-3">
        <div class={cn('flex shrink-0', sidebarOpen && 'md:hidden')}>
          <Button
            variant="ghost"
            size="icon-lg"
            class="text-muted-foreground md:hidden"
            onclick={() => (sessionsOpen = true)}
            aria-label="Show the conversations"
            title="Show the conversations"
          >
            <PanelLeftIcon class="size-[18px]" />
          </Button>
          <Button
            variant="ghost"
            size="icon-lg"
            class="text-muted-foreground max-md:hidden"
            onclick={() => (sidebarOpen = true)}
            aria-label="Show the conversations"
            title="Show the conversations"
          >
            <PanelLeftIcon class="size-[18px]" />
          </Button>
          <Button
            variant="ghost"
            size="icon-lg"
            class="text-muted-foreground"
            onclick={() => select(null)}
            aria-label="New conversation"
            title="New conversation"
          >
            <SquarePenIcon class="size-[18px]" />
          </Button>
        </div>

        {#if provider?.configured}
          {@const configured = provider.configured}
          <DropdownMenu.Root>
            <DropdownMenu.Trigger>
              {#snippet child({ props })}
                <button
                  {...props}
                  type="button"
                  class="hover:bg-muted aria-expanded:bg-muted flex min-w-0 items-center gap-2 rounded-lg px-3 py-1.5"
                  title="AI provider"
                >
                  <CpuIcon class="text-primary size-4 shrink-0" />
                  <span class="truncate font-medium">{configured.model}</span>
                  {#if needed}
                    <span class="text-destructive shrink-0">{needed}</span>
                  {:else}
                    <span class="text-muted-foreground shrink-0 max-sm:hidden">{serviceName}</span>
                  {/if}
                  <ChevronDownIcon class="text-muted-foreground size-4 shrink-0" />
                </button>
              {/snippet}
            </DropdownMenu.Trigger>
            <DropdownMenu.Content class="w-72 p-1.5">
              <DropdownMenu.Label class="text-muted-foreground px-2.5 text-xs font-normal">AI provider</DropdownMenu.Label>
              <div class="bg-muted flex items-start gap-2.5 rounded-lg px-2.5 py-2">
                <CpuIcon class="text-primary mt-0.5 size-4 shrink-0" />
                <div class="min-w-0">
                  <p class="truncate text-sm font-medium">{configured.model}</p>
                  <p class="text-muted-foreground truncate text-xs">
                    {serviceName}{configured.base_url ? ` · ${configured.base_url}` : ''}
                  </p>
                  {#if needed}
                    <p class="text-destructive text-xs">{needed}</p>
                  {/if}
                </div>
                {#if !needed}
                  <CheckIcon class="text-primary mt-0.5 ml-auto size-4 shrink-0" />
                {/if}
              </div>
              <DropdownMenu.Item class="mt-1 px-2.5 py-2" onSelect={() => (providerOpen = true)}>
                {provider.needs === 'sign_in' ? 'Sign in' : provider.needs === 'key' ? 'Enter the API key' : 'Change provider'}
              </DropdownMenu.Item>
            </DropdownMenu.Content>
          </DropdownMenu.Root>
        {:else}
          <Button variant="ghost" class="min-w-0" onclick={() => (providerOpen = true)}>
            <CpuIcon class="text-primary" /> <span class="truncate">Choose an AI provider</span>
          </Button>
        {/if}

        <div class="ml-auto flex shrink-0 items-center gap-1">
          {#if current}
            <Button
              variant="ghost"
              size="icon-lg"
              class="text-muted-foreground aria-pressed:bg-muted aria-pressed:text-foreground"
              aria-pressed={logOpen}
              onclick={() => (logOpen = !logOpen)}
              aria-label="Command log"
              title="Command log: what Kelpy ran in this conversation"
            >
              <SquareTerminalIcon class="size-[18px]" />
            </Button>
          {/if}
          {#if !usable()}
            <span class="text-destructive flex items-center gap-2 px-2 text-sm font-medium" role="status">
              <span class="bg-destructive size-2 rounded-full" aria-hidden="true"></span>
              {connection.reachable ? 'Session ended' : 'Disconnected'}
            </span>
          {/if}
          <ThemeToggle />
          <DropdownMenu.Root>
            <DropdownMenu.Trigger>
              {#snippet child({ props })}
                <Button
                  {...props}
                  variant="ghost"
                  size="icon-lg"
                  class="text-muted-foreground border-border rounded-full"
                  aria-label="Menu"
                  title="Sign out and Stop Kelpy"
                >
                  <SlidersHorizontalIcon class="size-[17px]" />
                </Button>
              {/snippet}
            </DropdownMenu.Trigger>
            <DropdownMenu.Content align="end" class="w-60 p-1.5">
              {#if status}
                <DropdownMenu.Label class="text-muted-foreground truncate px-2.5 text-xs font-normal" title={status.home}>
                  Kelpy v{status.version} · {status.home}
                </DropdownMenu.Label>
              {/if}
              <DropdownMenu.Item class="gap-2.5 px-2.5 py-2" onSelect={signOutAndForget}>
                <LogOutIcon /> Sign out
              </DropdownMenu.Item>
              <DropdownMenu.Separator />
              <DropdownMenu.Item class="gap-2.5 px-2.5 py-2" variant="destructive" onSelect={() => (stopOpen = true)}>
                <PowerIcon /> Stop Kelpy
              </DropdownMenu.Item>
            </DropdownMenu.Content>
          </DropdownMenu.Root>
        </div>
      </header>

      {#if !usable()}
        <div class="mx-auto w-full max-w-3xl px-4 pb-2">
          <Alert.Root variant="destructive">
            <Alert.Description>
              {#if connection.reachable}
                This page's session has ended: Kelpy has started again, or this browser was signed out. Open the link
                shown in the terminal where Kelpy runs. Text not sent yet stays in the message box.
              {:else}
                {@render unreachable()}
              {/if}
            </Alert.Description>
          </Alert.Root>
        </div>
      {/if}
      {#if current}
        <Chat
          bind:this={chat}
          session={current}
          start={start?.id === current.id ? start.message : null}
          onStarted={() => (start = null)}
          onChanged={keep}
          onOpenProvider={() => (providerOpen = true)}
          log={logOpen}
          onCloseLog={() => (logOpen = false)}
        />
      {:else}
        <NewConversation onCreated={started} />
      {/if}
    </main>
  </div>

  {#if provider}
    <ProviderDialog bind:open={providerOpen} info={provider} {afterChatGpt} onSaved={(info) => (provider = info)} />
  {/if}
  <StopDialog bind:open={stopOpen} onStopped={() => (view = { kind: 'stopped' })} />
  <Dialog.Root bind:open={sessionsOpen}>
    <Dialog.Content class="bg-sidebar flex h-[80dvh] flex-col gap-0 p-0">
      <Dialog.Title class="sr-only">Conversations</Dialog.Title>
      <div class="min-h-0 flex-1 pt-12">
        <Sessions {sessions} {selected} onSelect={select} onCreate={() => select(null)} onRenamed={keep} onDeleted={forget} />
      </div>
    </Dialog.Content>
  </Dialog.Root>
{:else}
  <div class="flex h-dvh flex-col">
    <header class="flex h-14 shrink-0 items-center px-3">
      {@render brand()}
      <div class="ml-auto"><ThemeToggle /></div>
    </header>
    {#if view.kind === 'stopped'}
      <main class="text-muted-foreground p-8">
        <p class="text-foreground text-lg font-medium">Kelpy has stopped.</p>
        <p class="mt-1 text-sm">You can close this page. To use Kelpy again, start it on the server.</p>
      </main>
    {:else if view.kind === 'sign-in'}
      <main class="px-4"><SignIn notice={view.notice} onSignedIn={() => load()} /></main>
    {:else if view.kind === 'error'}
      <main class="text-destructive p-8">{view.message}</main>
    {:else if !connection.reachable}
      <main class="text-muted-foreground p-8">{@render unreachable()}</main>
    {:else}
      <main class="text-muted-foreground p-8">Connecting…</main>
    {/if}
  </div>
{/if}
