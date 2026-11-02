<script lang="ts">
  import { untrack } from 'svelte';
  import ArrowLeftIcon from '@lucide/svelte/icons/arrow-left';
  import CreditCardIcon from '@lucide/svelte/icons/credit-card';
  import GiftIcon from '@lucide/svelte/icons/gift';
  import KeyRoundIcon from '@lucide/svelte/icons/key-round';
  import ServerIcon from '@lucide/svelte/icons/server';
  import { Button } from '$lib/components/ui/button';
  import * as Dialog from '$lib/components/ui/dialog';
  import { Input } from '$lib/components/ui/input';
  import { Label } from '$lib/components/ui/label';
  import ChatGptAccount from './ChatGptAccount.svelte';
  import OllamaPanel from './OllamaPanel.svelte';
  import {
    describe,
    fetchModels,
    saveProvider,
    type ModelList,
    type ModelQuery,
    type ProviderInfo,
    type Service,
    type Way,
  } from './api';

  interface Props {
    open: boolean;
    info: ProviderInfo;
    /** Set when the dialog opens after OpenAI's sign-in page, with what went wrong if anything did. */
    afterChatGpt: { error: string | null } | null;
    onSaved: (info: ProviderInfo) => void;
  }

  let { open = $bindable(), info, afterChatGpt, onSaved }: Props = $props();

  /** What each way of the first question looks like. */
  const WAY_ICONS = { plan: CreditCardIcon, free: GiftIcon, key: KeyRoundIcon, local: ServerIcon };
  /** How long typing in the key or the address pauses before the service is asked for its models. */
  const LIST_DELAY = 500;

  let way = $state<Way | null>(null);
  let service = $state<Service | null>(null);
  let model = $state('');
  let baseUrl = $state('');
  let apiKey = $state('');
  let chatgptSignedIn = $state(false);
  let busy = $state(false);
  let error = $state<string | null>(null);
  /** The service's models, once listed for the current key and address. */
  let listed = $state<ModelList | null>(null);
  let listing = $state(false);
  /** Numbers the lists asked for, so that only the latest one is shown. */
  let listings = 0;

  const choices = $derived(info.services.filter((candidate) => candidate.way === way));
  const kindInfo = $derived(info.kinds.find((candidate) => candidate.id === service?.kind));
  const configuredHere = $derived(info.configured !== null && configuredService()?.id === service?.id);
  const needsBaseUrl = $derived(service !== null && service.base_url === null && !kindInfo?.default_base_url);
  const keyMissing = $derived(
    (kindInfo?.requires_key || service?.way === 'free') &&
      apiKey.trim() === '' &&
      !(configuredHere && info.key !== 'missing') &&
      !(kindInfo?.key_variable && kindInfo.key_in_environment),
  );
  /** What to ask the service for its models, once its key and address are known; ChatGPT and Ollama list their own. */
  const modelQuery = $derived<ModelQuery | null>(
    service && kindInfo?.accepts_key && !keyMissing && !(needsBaseUrl && baseUrl.trim() === '')
      ? {
          kind: service.kind,
          service: service.id,
          base_url: baseUrl.trim() || undefined,
          api_key: apiKey.trim() || undefined,
        }
      : null,
  );
  const modelChoices = $derived(
    listed?.models ?? (service?.models ?? []).map((id) => ({ id, recommended: true })),
  );
  const recommended = $derived(modelChoices.filter((choice) => choice.recommended).map((choice) => choice.id));
  const ready = $derived(
    service !== null &&
      model.trim() !== '' &&
      !keyMissing &&
      (!kindInfo?.signs_in || chatgptSignedIn) &&
      (!needsBaseUrl || baseUrl.trim() !== ''),
  );

  /** The service of the configuration; Kelpy names it also for configurations saved without one. */
  function configuredService(): Service | undefined {
    return info.services.find((candidate) => candidate.id === info.configured?.service);
  }

  // Every time the dialog opens, start from what is configured, or from the first question. Untracked, so that
  // choices made while the dialog is open do not run this again.
  $effect(() => {
    if (open) {
      untrack(() => {
        const start = afterChatGpt ? info.services.find((candidate) => candidate.kind === 'chatgpt') : configuredService();
        way = start?.way ?? null;
        choose(start ?? null);
        error = afterChatGpt?.error ?? null;
      });
    }
  });

  // Lists the service's models whenever its key or address changes, once typing pauses.
  $effect(() => {
    const query = modelQuery;
    const number = ++listings;
    listed = null;
    listing = false;
    if (query === null) {
      return;
    }
    const timer = setTimeout(() => void listModels(query, number), LIST_DELAY);
    return () => clearTimeout(timer);
  });

  async function listModels(query: ModelQuery, number: number) {
    listing = true;
    let list: ModelList;
    try {
      list = await fetchModels(query);
    } catch (reason: unknown) {
      list = { models: (service?.models ?? []).map((id) => ({ id, recommended: true })), error: describe(reason) };
    }
    if (number === listings) {
      listed = list;
      listing = false;
    }
  }

  function chooseWay(chosen: Way) {
    way = chosen;
    const options = info.services.filter((candidate) => candidate.way === chosen);
    choose(options.length === 1 ? options[0] : null);
  }

  /** Switches service, with its configured model and address, or the suggested ones. */
  function choose(chosen: Service | null) {
    service = chosen;
    apiKey = '';
    error = null;
    const configured = chosen && info.configured && configuredService()?.id === chosen.id ? info.configured : null;
    model = configured?.model ?? chosen?.models[0] ?? '';
    baseUrl = configured && chosen?.base_url === null ? (configured.base_url ?? '') : '';
  }

  function back() {
    if (service && choices.length > 1) {
      choose(null);
    } else {
      way = null;
      choose(null);
    }
  }

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (!service) {
      return;
    }
    busy = true;
    error = null;
    try {
      const saved = await saveProvider({
        kind: service.kind,
        service: service.id,
        model,
        base_url: baseUrl.trim() || undefined,
        api_key: apiKey.trim() || undefined,
      });
      onSaved(saved);
      open = false;
    } catch (reason: unknown) {
      error = describe(reason);
    } finally {
      busy = false;
    }
  }
</script>

<Dialog.Root bind:open>
  <Dialog.Content class={['max-h-[90dvh] overflow-y-auto', kindInfo?.signs_in ? 'sm:max-w-3xl' : 'sm:max-w-lg']}>
    <Dialog.Header>
      <Dialog.Title>How does Kelpy connect to AI?</Dialog.Title>
      <Dialog.Description>
        The choice is saved in <code>config.toml</code>. API keys stay in memory until Kelpy stops; so do sign-ins, unless you save them.
      </Dialog.Description>
    </Dialog.Header>

    {#if way === null}
      <div class="grid gap-2">
        {#each info.ways as option (option.id)}
          {@const Icon = WAY_ICONS[option.id]}
          <button
            type="button"
            class="hover:bg-accent flex items-start gap-3 rounded-md border p-3 text-left"
            onclick={() => chooseWay(option.id)}
          >
            <Icon class="text-muted-foreground mt-0.5 size-5 shrink-0" aria-hidden="true" />
            <span>
              <span class="block text-sm font-medium">{option.name}</span>
              <span class="text-muted-foreground block text-xs">{option.description}</span>
            </span>
          </button>
        {/each}
      </div>
    {:else}
      <form class="grid gap-4" onsubmit={submit}>
        <Button type="button" variant="ghost" size="sm" class="justify-self-start" onclick={back}>
          <ArrowLeftIcon /> Back
        </Button>

        {#if service === null}
          <div class="grid gap-2">
            {#each choices as option (option.id)}
              <button
                type="button"
                class="hover:bg-accent rounded-md border p-3 text-left"
                onclick={() => choose(option)}
              >
                <span class="block text-sm font-medium">{option.name}</span>
                <span class="text-muted-foreground block text-xs">{option.description}</span>
              </button>
            {/each}
          </div>
        {:else}
          <div>
            <p class="text-sm font-medium">{service.name}</p>
            <p class="text-muted-foreground text-xs">{service.description}</p>
          </div>

          {#if kindInfo?.signs_in}
            <ChatGptAccount methods={info.sign_in_methods} bind:model bind:signedIn={chatgptSignedIn} />
          {:else if service.kind === 'ollama'}
            <OllamaPanel {service} bind:model bind:baseUrl />
          {:else}
            {#if service.steps.length > 0}
              <ol class="text-muted-foreground list-decimal space-y-1 pl-5 text-sm">
                {#each service.steps as step (step)}
                  <li>{step}</li>
                {/each}
              </ol>
            {/if}
            {#if service.key_page}
              <a class="text-sm underline" href={service.key_page} target="_blank" rel="noreferrer">
                Open {new URL(service.key_page).host}
              </a>
            {/if}

            {#if needsBaseUrl}
              <div class="grid gap-2">
                <Label for="provider-url">API address</Label>
                <Input
                  id="provider-url"
                  bind:value={baseUrl}
                  placeholder="http://10.0.0.5:8000/v1"
                  required
                  autocomplete="off"
                />
              </div>
            {/if}

            {#if kindInfo?.accepts_key}
              <div class="grid gap-2">
                <Label for="provider-key">API key{kindInfo.requires_key || service.way !== 'local' ? '' : ' (if the server needs one)'}</Label>
                <Input id="provider-key" type="password" bind:value={apiKey} autocomplete="off" />
                <p class="text-muted-foreground text-xs">
                  {#if configuredHere && info.key === 'session'}
                    A key entered earlier is in use. A new one replaces it.
                  {:else if configuredHere && info.key === 'environment'}
                    The key in {kindInfo.key_variable} is in use. A key entered here takes its place.
                  {:else if kindInfo.key_variable && kindInfo.key_in_environment}
                    Empty to use the key in {kindInfo.key_variable}.
                  {/if}
                </p>
              </div>
            {/if}

            <div class="grid gap-2">
              <Label for="provider-model">Model</Label>
              <Input
                id="provider-model"
                bind:value={model}
                list="provider-models"
                placeholder={recommended[0] ?? 'Model name'}
                required
                autocomplete="off"
              />
              <datalist id="provider-models">
                {#each modelChoices as choice (choice.id)}
                  <option value={choice.id} label={choice.recommended ? 'Recommended' : undefined}></option>
                {/each}
              </datalist>
              <p class="text-muted-foreground text-xs">
                {#if recommended.length > 0}Recommended: {recommended.join(', ')}.{/if}
                {#if listing}
                  Listing the service's models…
                {:else if listed?.error}
                  The service's list is unavailable: {listed.error}. Any model name can be typed.
                {:else if listed}
                  {listed.models.length - recommended.length}
                  {recommended.length > 0 ? 'more' : 'models'} listed by the service: type to search.
                {/if}
              </p>
            </div>
          {/if}

          {#if error}
            <p class="text-destructive text-sm" role="alert">{error}</p>
          {/if}

          <Dialog.Footer>
            <Button type="button" variant="outline" onclick={() => (open = false)}>Cancel</Button>
            <Button type="submit" disabled={busy || !ready}>Save</Button>
          </Dialog.Footer>
        {/if}
      </form>
    {/if}
  </Dialog.Content>
</Dialog.Root>
