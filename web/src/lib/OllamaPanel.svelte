<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { Button } from '$lib/components/ui/button';
  import { Input } from '$lib/components/ui/input';
  import { Label } from '$lib/components/ui/label';
  import {
    describe,
    downloadModel,
    fetchOllama,
    fetchOllamaInstaller,
    gigabytes,
    installOllama,
    type Download,
    type OllamaInstallation,
    type OllamaInstaller,
    type OllamaStatus,
    type Service,
  } from './api';

  interface Props {
    service: Service;
    /** The chosen model's name. */
    model: string;
    /** The address typed, empty for Ollama's default one. */
    baseUrl: string;
  }

  let { service, model = $bindable(), baseUrl = $bindable() }: Props = $props();

  let status = $state<OllamaStatus | null>(null);
  let checking = $state(false);
  let error = $state<string | null>(null);
  /** Downloads followed by this panel, by address and model. */
  let downloads = $state<Record<string, Download>>({});
  const key = (address: string, name: string) => `${address} ${name}`;
  const following = new AbortController();

  const installed = $derived(new Set(status?.models.map((candidate) => candidate.name) ?? []));
  const suggestions = $derived(status?.suggestions.filter((candidate) => !installed.has(candidate.name)) ?? []);

  /** The number of the latest check: an answer to an earlier one, or for an address no longer typed, is dropped. */
  let checks = 0;

  async function check() {
    const number = ++checks;
    const address = baseUrl.trim();
    const latest = () => number === checks && baseUrl.trim() === address;
    checking = true;
    error = null;
    try {
      const answer = await fetchOllama(address);
      if (!latest()) {
        return;
      }
      status = answer;
      if (status.running && !installed.has(model)) {
        model = status.models[0]?.name ?? '';
      }
      for (const download of status.downloads) {
        if (download.base_url === status.base_url && !(key(download.base_url, download.model) in downloads)) {
          void follow(download.model);
        }
      }
    } catch (reason: unknown) {
      if (latest()) {
        error = describe(reason);
      }
    } finally {
      if (number === checks) {
        checking = false;
      }
    }
  }

  /**
   * Starts the download of `name` into the Ollama shown, or follows the one under way, until it ends. The model is
   * chosen then only if the panel still shows that Ollama: the address may have changed meanwhile.
   */
  async function follow(name: string) {
    if (!status) {
      return;
    }
    const address = status.base_url;
    const id = key(address, name);
    downloads[id] = {
      base_url: address,
      model: name,
      status: 'starting',
      completed: 0,
      total: 0,
      done: false,
      error: null,
    };
    try {
      await downloadModel(address, name, (progress) => (downloads[id] = progress), following.signal);
    } catch (reason: unknown) {
      if (!following.signal.aborted) {
        downloads[id] = { ...downloads[id], done: true, error: describe(reason) };
      }
      return;
    }
    if (!downloads[id].error) {
      delete downloads[id];
      await check();
      if (status?.base_url === address && installed.has(name)) {
        model = name;
      }
    }
  }

  /** How Ollama would be installed here, once asked; then its installation as it goes. */
  let installer = $state<OllamaInstaller | null>(null);
  let installation = $state<OllamaInstallation | null>(null);

  async function offerInstall() {
    error = null;
    try {
      installer = await fetchOllamaInstaller();
    } catch (reason: unknown) {
      error = describe(reason);
    }
  }

  async function install() {
    installation = { command: installer?.command ?? '', output: '', done: false, error: null };
    try {
      await installOllama((progress) => (installation = progress), following.signal);
    } catch (reason: unknown) {
      if (!following.signal.aborted && installation) {
        installation = { ...installation, done: true, error: describe(reason) };
      }
      return;
    }
    if (!installation?.error) {
      installer = null;
      installation = null;
      await check();
    }
  }

  onMount(() => void check());
  // Closing the dialog stops following; the downloads go on.
  onDestroy(() => following.abort());
</script>

<div class="grid gap-2">
  <Label for="ollama-url">Address (optional)</Label>
  <div class="flex gap-2">
    <Input
      id="ollama-url"
      bind:value={baseUrl}
      placeholder={status?.base_url ?? 'http://localhost:11434/v1'}
      autocomplete="off"
      onchange={() => void check()}
    />
    <Button type="button" variant="outline" disabled={checking} onclick={() => void check()}>Check</Button>
  </div>
  <p class="text-muted-foreground text-xs">Empty for Ollama on this computer. For another computer: http://address:11434/v1.</p>
</div>

{#if status === null}
  <p class="text-muted-foreground text-sm">{checking ? 'Checking Ollama…' : ''}</p>
{:else if !status.running}
  <p class="text-sm" role="status">
    {#if status.installed}
      Ollama is installed but does not answer at {status.base_url}. Start it, then choose Check.
    {:else if status.memory === null}
      No Ollama answers at {status.base_url}.
    {:else}
      Ollama is not installed on this computer. Kelpy can install it, or download it from
      <a class="underline" href="https://ollama.com/download" target="_blank" rel="noreferrer">ollama.com/download</a>,
      or enter the address of a computer running it.
    {/if}
  </p>
  {#if !status.installed && status.memory !== null}
    {#if installation}
      <div class="grid gap-1.5">
        <p class="text-sm">
          {#if !installation.done}Installing Ollama…{:else if installation.error}The installation did not complete: {installation.error}{/if}
        </p>
        <pre class="bg-muted max-h-48 overflow-auto rounded-lg px-3 py-2 font-mono text-xs">{installation.output || installation.command}</pre>
      </div>
    {:else if installer}
      <div class="grid gap-1.5 rounded-lg border p-3">
        <p class="text-sm font-medium">Kelpy will run:</p>
        <pre class="bg-muted overflow-x-auto rounded-lg px-3 py-2 font-mono text-xs whitespace-pre-wrap">{installer.command}</pre>
        <ul class="text-muted-foreground space-y-0.5 text-xs">
          {#each installer.notes as note (note)}<li>{note}</li>{/each}
        </ul>
        <div class="flex gap-2">
          <Button type="button" size="sm" onclick={() => void install()}>Install</Button>
          <Button type="button" size="sm" variant="outline" onclick={() => (installer = null)}>Cancel</Button>
        </div>
      </div>
    {:else}
      <div>
        <Button type="button" variant="outline" size="sm" onclick={() => void offerInstall()}>Install Ollama</Button>
      </div>
    {/if}
  {/if}
{:else}
  <fieldset class="grid gap-1">
    <legend class="mb-1 text-sm font-medium">Installed models</legend>
    {#each status.models as candidate (candidate.name)}
      <label class="flex items-center gap-2 text-sm">
        <input type="radio" name="ollama-model" value={candidate.name} bind:group={model} />
        <span class="font-mono">{candidate.name}</span>
        <span class="text-muted-foreground">
          {candidate.cloud ? 'runs on ollama.com' : gigabytes(candidate.size)}
        </span>
      </label>
    {:else}
      <p class="text-muted-foreground text-sm">No models installed.</p>
    {/each}
  </fieldset>

  {#if suggestions.length > 0}
    <div class="grid gap-2">
      <p class="text-sm font-medium">
        Suggested{status.memory === null ? '' : ` for this computer (${gigabytes(status.memory)} of RAM)`}
      </p>
      {#each suggestions as suggestion (suggestion.name)}
        {@const download = downloads[key(status.base_url, suggestion.name)]}
        <div class="grid gap-1 rounded-md border p-2 text-sm">
          <div class="flex items-center gap-2">
            <span class="font-mono">{suggestion.name}</span>
            <span class="text-muted-foreground">{gigabytes(suggestion.size)}</span>
            <Button
              type="button"
              size="sm"
              variant="outline"
              class="ml-auto"
              disabled={download !== undefined && !download.done}
              onclick={() => void follow(suggestion.name)}
            >
              {download?.error ? 'Retry' : 'Download'}
            </Button>
          </div>
          <p class="text-muted-foreground text-xs">{suggestion.description}</p>
          {#if download && !download.done}
            <progress class="w-full" max={download.total || 1} value={download.completed}></progress>
            <p class="text-muted-foreground text-xs">
              {download.status}{download.total ? ` · ${gigabytes(download.completed)} of ${gigabytes(download.total)}` : ''}
            </p>
          {:else if download?.error}
            <p class="text-destructive text-xs" role="alert">Download failed: {download.error}</p>
          {/if}
        </div>
      {/each}
    </div>
  {/if}

  {#each service.steps as step (step)}
    <p class="text-muted-foreground text-xs">{step}</p>
  {/each}
{/if}

{#if error}
  <p class="text-destructive text-sm" role="alert">{error}</p>
{/if}
