<script lang="ts">
  import { untrack } from 'svelte';
  import UploadIcon from '@lucide/svelte/icons/upload';
  import * as Alert from '$lib/components/ui/alert';
  import { Badge } from '$lib/components/ui/badge';
  import { Button } from '$lib/components/ui/button';
  import * as Dialog from '$lib/components/ui/dialog';
  import { deleteFile, describe, fetchFile, fetchFiles, uploadFile, type UploadedFile } from './api';
  import { copyText } from './clipboard';
  import { downloadBlob } from './download';
  import { size } from './files';

  interface Props {
    open: boolean;
    /** Counts the changes Kelpy announced: each one reads the files again. */
    changes: number;
  }

  let { open = $bindable(), changes }: Props = $props();

  let files = $state<UploadedFile[] | null>(null);
  let error = $state<string | null>(null);
  let uploading = $state(0);
  let dragging = $state(false);
  let copied = $state<string | null>(null);
  let picker = $state<HTMLInputElement | null>(null);

  $effect(() => {
    if (open) {
      void changes;
      untrack(() => void load());
    }
  });

  async function load() {
    try {
      files = await fetchFiles();
    } catch (reason: unknown) {
      error = describe(reason);
    }
  }

  async function upload(chosen: FileList | null) {
    error = null;
    for (const file of chosen ?? []) {
      uploading += 1;
      try {
        await uploadFile(file);
      } catch (reason: unknown) {
        error = `${file.name}: ${describe(reason)}`;
      } finally {
        uploading -= 1;
      }
    }
    await load();
  }

  async function run(work: () => Promise<void>) {
    error = null;
    try {
      await work();
    } catch (reason: unknown) {
      error = describe(reason);
    }
  }

  function drop(event: DragEvent) {
    event.preventDefault();
    dragging = false;
    void upload(event.dataTransfer?.files ?? null);
  }
</script>

<Dialog.Root bind:open>
  <Dialog.Content class="max-h-[88dvh] overflow-y-auto sm:max-w-xl">
    <Dialog.Header>
      <Dialog.Title>Files</Dialog.Title>
      <Dialog.Description>
        Files you upload for Kelpy to use, such as a configuration, a database dump or a certificate. They are kept on
        the server in Kelpy's workspace, readable by Kelpy's account only, and the model reads them there.
      </Dialog.Description>
    </Dialog.Header>

    {#if error}
      <Alert.Root variant="destructive">
        <Alert.Description>{error}</Alert.Description>
      </Alert.Root>
    {/if}

    <div
      role="region"
      aria-label="Upload"
      class="text-muted-foreground grid place-items-center gap-2 rounded-lg border border-dashed p-5 text-sm {dragging
        ? 'border-primary bg-primary/5'
        : ''}"
      ondragover={(event) => {
        event.preventDefault();
        dragging = true;
      }}
      ondragleave={() => (dragging = false)}
      ondrop={drop}
    >
      <span>Drop files here, or</span>
      <Button variant="outline" onclick={() => picker?.click()} disabled={uploading > 0}>
        <UploadIcon /> {uploading > 0 ? 'Uploading…' : 'Choose files'}
      </Button>
      <span class="text-xs">Up to 2 GiB each. A name already taken gets a number.</span>
      <input
        bind:this={picker}
        type="file"
        multiple
        class="hidden"
        onchange={(event) => {
          void upload(event.currentTarget.files);
          event.currentTarget.value = '';
        }}
      />
    </div>

    {#if files?.length === 0}
      <p class="text-muted-foreground text-sm">No files uploaded.</p>
    {:else if files}
      <ul class="grid grid-cols-[minmax(0,1fr)] gap-1">
        {#each files as file (file.name)}
          <li class="flex flex-wrap items-center gap-x-2 gap-y-1 text-sm">
            <span class="min-w-0 truncate" title={file.path}>{file.name}</span>
            <span class="text-muted-foreground shrink-0 text-xs">{size(file.size)}</span>
            {#if !file.text}
              <Badge variant="outline" title="The model can use it in commands, not read it">Binary</Badge>
            {/if}
            <span class="ml-auto flex shrink-0 gap-1">
              <Button
                variant="ghost"
                size="sm"
                onclick={() =>
                  run(async () => {
                    if (await copyText(file.path)) {
                      copied = file.name;
                      setTimeout(() => copied === file.name && (copied = null), 1500);
                    }
                  })}
              >
                {copied === file.name ? 'Copied' : 'Copy path'}
              </Button>
              <Button variant="ghost" size="sm" onclick={() => run(async () => downloadBlob(file.name, await fetchFile(file.name)))}>
                Download
              </Button>
              <Button variant="ghost" size="sm" onclick={() => run(async () => { await deleteFile(file.name); await load(); })}>
                Delete
              </Button>
            </span>
          </li>
        {/each}
      </ul>
    {/if}
  </Dialog.Content>
</Dialog.Root>
