<script lang="ts">
  import EllipsisIcon from '@lucide/svelte/icons/ellipsis';
  import PencilIcon from '@lucide/svelte/icons/pencil';
  import SquarePenIcon from '@lucide/svelte/icons/square-pen';
  import Trash2Icon from '@lucide/svelte/icons/trash-2';
  import { Button } from '$lib/components/ui/button';
  import * as Dialog from '$lib/components/ui/dialog';
  import * as DropdownMenu from '$lib/components/ui/dropdown-menu';
  import { Input } from '$lib/components/ui/input';
  import { cn } from '$lib/utils';
  import { deleteSession, describe, renameSession, type Session } from './api';
  import { groupByDate } from './dates';

  interface Props {
    sessions: Session[];
    selected: number | null;
    onSelect: (id: number) => void;
    onCreate: () => void;
    onRenamed: (session: Session) => void;
    onDeleted: (id: number) => void;
  }

  let { sessions, selected, onSelect, onCreate, onRenamed, onDeleted }: Props = $props();

  // The day is read when the list changes, which happens after every answer.
  const groups = $derived(groupByDate(sessions, new Date()));

  let renaming = $state<Session | null>(null);
  let deleting = $state<Session | null>(null);
  let title = $state('');
  let error = $state<string | null>(null);

  function startRename(session: Session) {
    renaming = session;
    title = session.title;
    error = null;
  }

  async function rename(event: SubmitEvent) {
    event.preventDefault();
    if (!renaming) {
      return;
    }
    try {
      onRenamed(await renameSession(renaming.id, title));
      renaming = null;
    } catch (reason: unknown) {
      error = describe(reason);
    }
  }

  async function remove() {
    if (!deleting) {
      return;
    }
    const id = deleting.id;
    try {
      await deleteSession(id);
      deleting = null;
      onDeleted(id);
    } catch (reason: unknown) {
      error = describe(reason);
    }
  }
</script>

<div class="flex h-full min-h-0 flex-col">
  <div class="px-2">
    <button
      type="button"
      class="hover:bg-muted flex w-full items-center gap-2.5 rounded-lg px-3 py-2 text-sm"
      onclick={onCreate}
    >
      <SquarePenIcon class="size-4" /> New conversation
    </button>
  </div>
  <nav class="min-h-0 flex-1 overflow-y-auto px-2 pb-3" aria-label="Conversations">
    {#each groups as group (group.name)}
      <h2 class="text-muted-foreground px-3 pt-5 pb-1.5 text-xs font-medium">{group.name}</h2>
      <ul class="flex flex-col gap-px">
        {#each group.items as session (session.id)}
          <li class="group relative">
            <button
              type="button"
              class={cn(
                'hover:bg-muted w-full truncate rounded-lg py-2 pr-8 pl-3 text-left text-sm',
                session.id === selected && 'bg-sidebar-accent hover:bg-sidebar-accent font-medium',
              )}
              aria-current={session.id === selected ? 'page' : undefined}
              onclick={() => onSelect(session.id)}
            >
              {session.title}
            </button>
            <!-- Without a mouse to hover with, the actions stay visible. -->
            <div
              class="absolute top-1/2 right-1.5 -translate-y-1/2 opacity-0 group-hover:opacity-100 focus-within:opacity-100 has-aria-expanded:opacity-100 pointer-coarse:opacity-100"
            >
              <DropdownMenu.Root>
                <DropdownMenu.Trigger>
                  {#snippet child({ props })}
                    <Button
                      {...props}
                      variant="ghost"
                      size="icon-sm"
                      class="text-muted-foreground hover:text-foreground hover:bg-transparent"
                      aria-label="Actions for {session.title}"
                      title="Rename or delete"
                    >
                      <EllipsisIcon />
                    </Button>
                  {/snippet}
                </DropdownMenu.Trigger>
                <DropdownMenu.Content align="start" class="w-40">
                  <DropdownMenu.Item onSelect={() => startRename(session)}><PencilIcon /> Rename</DropdownMenu.Item>
                  <DropdownMenu.Item
                    variant="destructive"
                    onSelect={() => {
                      deleting = session;
                      error = null;
                    }}
                  >
                    <Trash2Icon /> Delete
                  </DropdownMenu.Item>
                </DropdownMenu.Content>
              </DropdownMenu.Root>
            </div>
          </li>
        {/each}
      </ul>
    {:else}
      <p class="text-muted-foreground px-3 pt-5 text-sm">No conversations yet.</p>
    {/each}
  </nav>
</div>

<Dialog.Root open={renaming !== null} onOpenChange={(open) => !open && (renaming = null)}>
  <Dialog.Content class="sm:max-w-md">
    <Dialog.Header>
      <Dialog.Title>Rename conversation</Dialog.Title>
    </Dialog.Header>
    <form class="grid gap-4" onsubmit={rename}>
      <Input bind:value={title} aria-label="Title" required />
      {#if error}
        <p class="text-destructive text-sm" role="alert">{error}</p>
      {/if}
      <Dialog.Footer>
        <Button type="button" variant="outline" onclick={() => (renaming = null)}>Cancel</Button>
        <Button type="submit" disabled={title.trim() === ''}>Rename</Button>
      </Dialog.Footer>
    </form>
  </Dialog.Content>
</Dialog.Root>

<Dialog.Root open={deleting !== null} onOpenChange={(open) => !open && (deleting = null)}>
  <Dialog.Content class="sm:max-w-md">
    <Dialog.Header>
      <Dialog.Title>Delete this conversation?</Dialog.Title>
      <Dialog.Description>
        “{deleting?.title}” and all its messages will be deleted. This cannot be undone.
      </Dialog.Description>
    </Dialog.Header>
    {#if error}
      <p class="text-destructive text-sm" role="alert">{error}</p>
    {/if}
    <Dialog.Footer>
      <Button variant="outline" onclick={() => (deleting = null)}>Cancel</Button>
      <Button variant="destructive" onclick={remove}>Delete</Button>
    </Dialog.Footer>
  </Dialog.Content>
</Dialog.Root>
