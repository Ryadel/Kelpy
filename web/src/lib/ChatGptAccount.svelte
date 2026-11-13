<script lang="ts">
  import { onMount } from 'svelte';
  import BotIcon from '@lucide/svelte/icons/bot';
  import CheckIcon from '@lucide/svelte/icons/check';
  import CopyIcon from '@lucide/svelte/icons/copy';
  import TerminalIcon from '@lucide/svelte/icons/terminal';
  import UploadIcon from '@lucide/svelte/icons/upload';
  import { Button } from '$lib/components/ui/button';
  import * as Dialog from '$lib/components/ui/dialog';
  import { Label } from '$lib/components/ui/label';
  import * as Select from '$lib/components/ui/select';
  import { Switch } from '$lib/components/ui/switch';
  import {
    describe,
    fetchAgentFile,
    fetchChatGpt,
    fetchChatGptModels,
    forgetRegistration,
    signInWithCode,
    signOutOfChatGpt,
    startChatGptSignIn,
    useAgentFile,
    type ChatGptModel,
    type ChatGptStatus,
    type SignInMethod,
  } from './api';
  import { copyText } from './clipboard';

  interface Props {
    /** The ways to sign in, in the order shown. */
    methods: SignInMethod[];
    /** The chosen model's slug. */
    model: string;
    /** Whether a ChatGPT session is open, so that the model can be saved. */
    signedIn: boolean;
  }

  let { methods, model = $bindable(), signedIn = $bindable() }: Props = $props();

  let status = $state<ChatGptStatus | null>(null);
  let models = $state<ChatGptModel[]>([]);
  let busy = $state(false);
  let error = $state<string | null>(null);
  let notice = $state<string | null>(null);
  /** The hidden file chooser behind "Upload registration file". */
  let registrationInput = $state<HTMLInputElement | null>(null);
  /** The code being approved, while a sign-in as the Codex CLI waits for it. */
  let code = $state<{ url: string; userCode: string; qr: string } | null>(null);
  /** Whether the code was just copied: the button says so for a moment. */
  let codeCopied = $state(false);

  /** Copies the code to paste on ChatGPT's page. */
  async function copyCode(userCode: string) {
    if (await copyText(userCode)) {
      codeCopied = true;
      setTimeout(() => (codeCopied = false), 2000);
    }
  }
  let waiting: AbortController | null = null;
  /**
   * The next sign-in stays across restarts. Turning it on opens `confirming`: the sign-in then stays on disk, usable by
   * whoever can read the Kelpy folder. Closing that without confirming turns it off again.
   */
  let keep = $state(false);
  let confirming = $state(false);

  const chosen = $derived(models.find((candidate) => candidate.slug === model));

  async function refresh() {
    try {
      status = await fetchChatGpt();
      signedIn = status.signed_in !== null;
      models = signedIn ? await fetchChatGptModels() : [];
      if (signedIn && !models.some((candidate) => candidate.slug === model)) {
        model = models[0]?.slug ?? '';
      }
    } catch (reason: unknown) {
      error = describe(reason);
    }
  }

  /** Asks OpenAI for a code, shows it, and waits until it is approved on any device. */
  async function signInAsCodex() {
    waiting?.abort();
    const controller = new AbortController();
    waiting = controller;
    busy = true;
    error = null;
    notice = null;
    try {
      await signInWithCode((event) => {
        if (event.type === 'code') {
          code = { url: event.verification_url, userCode: event.user_code, qr: event.qr };
        } else if (event.type === 'failed') {
          error = event.error;
        }
      }, keep, controller.signal);
      await refresh();
    } catch (reason: unknown) {
      if (!controller.signal.aborted) {
        error = describe(reason);
      }
    } finally {
      if (waiting === controller) {
        waiting = null;
        code = null;
        busy = false;
      }
    }
  }

  function closeConfirm(accepted: boolean) {
    keep = accepted;
    confirming = false;
  }

  function cancelCode() {
    waiting?.abort();
  }

  /** Leaves for OpenAI's sign-in page, which sends the browser back to this Kelpy. */
  async function signInAsAgent() {
    busy = true;
    error = null;
    try {
      const { url } = await startChatGptSignIn(keep);
      window.location.assign(url);
    } catch (reason: unknown) {
      error = describe(reason);
      busy = false;
    }
  }

  async function downloadRegistration() {
    error = null;
    try {
      const { text } = await fetchAgentFile();
      const link = document.createElement('a');
      link.href = URL.createObjectURL(new Blob([text], { type: 'application/toml' }));
      link.download = 'kelpy-agent.toml';
      link.click();
      URL.revokeObjectURL(link.href);
    } catch (reason: unknown) {
      error = describe(reason);
    }
  }

  async function useRegistration(event: Event) {
    const input = event.currentTarget as HTMLInputElement;
    const file = input.files?.[0];
    input.value = '';
    if (!file) {
      return;
    }
    busy = true;
    error = null;
    try {
      await useAgentFile(await file.text());
      await refresh();
    } catch (reason: unknown) {
      error = describe(reason);
    } finally {
      busy = false;
    }
  }

  async function forget() {
    busy = true;
    error = null;
    notice = null;
    try {
      status = await forgetRegistration();
    } catch (reason: unknown) {
      error = describe(reason);
    } finally {
      busy = false;
    }
  }

  async function signOut() {
    busy = true;
    error = null;
    notice = null;
    try {
      const { revoked, forgotten } = await signOutOfChatGpt();
      if (!forgotten && !revoked) {
        // The one outcome that signs in again: the next start restores the session left on disk.
        error =
          'Signed out, but Kelpy could not delete data/chatgpt-session.toml in its home, and OpenAI did not confirm ' +
          'the revocation. Delete the file, or the next start signs in again.';
      } else if (!forgotten) {
        notice = 'Signed out. Kelpy could not delete data/chatgpt-session.toml in its home; the session in it no longer works.';
      } else {
        notice = revoked ? null : 'Signed out. OpenAI did not confirm the revocation; the session expires within 30 days.';
      }
      await refresh();
    } catch (reason: unknown) {
      error = describe(reason);
    } finally {
      busy = false;
    }
  }

  onMount(() => {
    void refresh();
    // Closing the setup abandons a sign-in with a code.
    return () => waiting?.abort();
  });
</script>

{#if status === null}
  <p class="text-muted-foreground text-sm">Checking the sign-in…</p>
{:else if status.signed_in}
  <div class="grid gap-2">
    <Label for="chatgpt-model">Model</Label>
    <Select.Root type="single" value={model} onValueChange={(value) => (model = value)}>
      <Select.Trigger id="chatgpt-model" class="w-full">{chosen?.display_name ?? 'Choose a model'}</Select.Trigger>
      <Select.Content>
        {#each models as option (option.slug)}
          <Select.Item value={option.slug} label={option.display_name}>{option.display_name}</Select.Item>
        {/each}
      </Select.Content>
    </Select.Root>
  </div>
  <div class="flex flex-wrap items-center justify-between gap-2 text-sm">
    <span class="text-muted-foreground truncate">
      Signed in as {status.signed_in.email ?? 'your ChatGPT account'}
      ({status.method === 'codex' ? 'Codex CLI' : 'Agent'})
      {status.kept ? '(saved - log out to clear)' : '(not saved - close to clear)'}
    </span>
    <div class="flex gap-2">
      {#if status.method === 'agent'}
        <Button type="button" variant="ghost" size="sm" onclick={downloadRegistration}>Download registration file</Button>
      {/if}
      <Button type="button" variant="outline" size="sm" disabled={busy} onclick={signOut}>Sign out</Button>
    </div>
  </div>
{:else}
  <div class="grid gap-3 md:grid-cols-2">
    {#each methods as method (method.id)}
      <section class="grid content-start gap-2 rounded-md border p-3">
        <h3 class="flex items-center gap-2 text-sm font-medium">
          {#if method.id === 'codex'}
            <TerminalIcon class="text-muted-foreground size-5" aria-hidden="true" />
          {:else}
            <BotIcon class="text-muted-foreground size-5" aria-hidden="true" />
          {/if}
          {method.name}
        </h3>
        <p class="text-xs">Recommended {method.recommended}</p>
        <p class="text-muted-foreground text-xs"><span class="text-foreground">Pros.</span> {method.pros}</p>
        <p class="text-muted-foreground text-xs"><span class="text-foreground">Cons.</span> {method.cons}</p>

        {#if method.id === 'codex'}
          {#if code}
            <div class="grid justify-items-center gap-2 pt-1 text-center">
              <img
                class="size-36 rounded bg-white p-1"
                src={`data:image/svg+xml;charset=utf-8,${encodeURIComponent(code.qr)}`}
                alt={`QR code of ${code.url}`}
              />
              <p class="text-xs">
                Open <a class="underline" href={code.url} target="_blank" rel="noreferrer">{code.url}</a> on any device,
                sign in to ChatGPT and enter:
              </p>
              <div class="flex items-center gap-1">
                <p class="font-mono text-lg tracking-widest">{code.userCode}</p>
                <Button
                  type="button"
                  variant="ghost"
                  size="sm"
                  aria-label="Copy the code"
                  onclick={() => copyCode(code?.userCode ?? '')}
                >
                  {#if codeCopied}<CheckIcon class="size-4" />Copied{:else}<CopyIcon class="size-4" />Copy{/if}
                </Button>
              </div>
              <p class="text-muted-foreground text-xs">The code expires in 15 minutes. This page continues by itself.</p>
              <Button type="button" variant="outline" size="sm" onclick={cancelCode}>Cancel</Button>
            </div>
          {:else}
            <Button type="button" class="mt-1" disabled={busy} onclick={signInAsCodex}>Get a code</Button>
          {/if}
        {:else}
          <Button type="button" class="mt-1" disabled={busy} onclick={signInAsAgent}>
            Sign in with ChatGPT
          </Button>
          <p class="text-muted-foreground text-xs">Sign-in happens on OpenAI's site, which then returns to this page.</p>
          {#if status.registered}
            <p class="text-muted-foreground text-xs">
              Registered in {status.registered}'s ChatGPT account.
              <Button type="button" variant="link" class="h-auto p-0 text-xs" disabled={busy} onclick={forget}>
                Forget registration
              </Button>
              ·
              <Button type="button" variant="link" class="h-auto p-0 text-xs" onclick={downloadRegistration}>
                Download registration file
              </Button>
            </p>
          {/if}
          <div class="grid justify-items-start gap-1">
            <Button type="button" variant="outline" size="sm" disabled={busy} onclick={() => registrationInput?.click()}>
              <UploadIcon aria-hidden="true" />
              Upload registration file…
            </Button>
            <input bind:this={registrationInput} class="hidden" type="file" accept=".toml,text/plain" onchange={useRegistration} />
            <p class="text-muted-foreground text-xs">Downloaded from another Kelpy.</p>
          </div>
        {/if}
      </section>
    {/each}
  </div>
  <div class="grid gap-1">
    <div class="flex items-center gap-2">
      <Switch id="chatgpt-keep" bind:checked={keep} onCheckedChange={(on) => (confirming = on)} disabled={busy} />
      <Label for="chatgpt-keep" class="font-normal">Save the sign-in on this computer</Label>
    </div>
    <p class="text-muted-foreground text-xs">
      Recommended off: Kelpy then asks to sign in after each restart. Leave it off on a server you hand over to others.
    </p>
  </div>
{/if}
<!-- Escape, the overlay and the close button count as Cancel; after a choice `confirming` is already false. -->
<Dialog.Root open={confirming} onOpenChange={(open) => !open && confirming && closeConfirm(false)}>
  <Dialog.Content class="sm:max-w-md">
    <Dialog.Header>
      <Dialog.Title>Save the sign-in on this computer?</Dialog.Title>
      <Dialog.Description>
        Kelpy saves the ChatGPT sign-in in its folder, in <code>data/</code>, readable only by your user. Anyone who
        can read that folder as you or as an administrator can use your ChatGPT plan until you sign out. Signing out
        removes it.
      </Dialog.Description>
    </Dialog.Header>
    <Dialog.Footer>
      <Button type="button" variant="outline" onclick={() => closeConfirm(false)}>Cancel</Button>
      <Button type="button" onclick={() => closeConfirm(true)}>Save</Button>
    </Dialog.Footer>
  </Dialog.Content>
</Dialog.Root>
{#if notice}
  <p class="text-muted-foreground text-sm">{notice}</p>
{/if}
{#if error}
  <p class="text-destructive text-sm" role="alert">{error}</p>
{/if}
