<script lang="ts" generics="T extends string">
  import type { Component } from 'svelte';
  import ChevronDownIcon from '@lucide/svelte/icons/chevron-down';
  import * as DropdownMenu from '$lib/components/ui/dropdown-menu';

  interface Option {
    value: T;
    name: string;
    description: string;
    /** Shown but not chosen, such as a level not available yet. */
    disabled?: boolean;
  }

  interface Props {
    /** What the menu chooses, such as "System access". */
    label: string;
    /** What it does, shown on the button. */
    title: string;
    icon: Component;
    value: T;
    options: Option[];
    /** Shown under the options, such as what the choice depends on. */
    note?: string;
    /** A button under the note, such as restarting Kelpy as administrator. */
    noteAction?: { label: string; icon: Component; onclick: () => void };
    disabled?: boolean;
    onChange: (value: T) => void;
  }

  let { label, title, icon: Icon, value, options, note, noteAction, disabled = false, onChange }: Props = $props();

  const current = $derived(options.find((option) => option.value === value));
</script>

<DropdownMenu.Root>
  <DropdownMenu.Trigger {disabled}>
    {#snippet child({ props })}
      <button
        {...props}
        type="button"
        {title}
        aria-label="{label}: {current?.name}"
        class="text-muted-foreground hover:bg-muted hover:text-foreground aria-expanded:bg-muted inline-flex items-center gap-1.5 rounded-full border px-3 py-1 text-[13px] disabled:pointer-events-none disabled:opacity-45"
      >
        <Icon class="size-4" />
        {current?.name}
        <ChevronDownIcon class="size-3.5" />
      </button>
    {/snippet}
  </DropdownMenu.Trigger>
  <DropdownMenu.Content align="end" class="w-80 max-w-[calc(100vw-1rem)] p-1.5">
    <DropdownMenu.Label class="text-muted-foreground px-2.5 text-xs font-normal">{label}</DropdownMenu.Label>
    <DropdownMenu.RadioGroup {value} onValueChange={(chosen) => onChange(chosen as T)}>
      {#each options as option (option.value)}
        <DropdownMenu.RadioItem value={option.value} disabled={option.disabled} closeOnSelect class="items-start py-2">
          <div class="grid gap-0.5">
            <span class="font-medium">{option.name}</span>
            <span class="text-muted-foreground text-xs leading-snug">{option.description}</span>
          </div>
        </DropdownMenu.RadioItem>
      {/each}
    </DropdownMenu.RadioGroup>
    {#if note}
      <DropdownMenu.Separator />
      <p class="text-muted-foreground px-2.5 py-1.5 text-xs leading-snug">{note}</p>
      {#if noteAction}
        {@const ActionIcon = noteAction.icon}
        <DropdownMenu.Item
          onSelect={noteAction.onclick}
          class="bg-primary text-primary-foreground data-highlighted:bg-primary/90 data-highlighted:text-primary-foreground mx-1 mb-1 justify-center font-medium"
        >
          <ActionIcon class="size-4" />
          {noteAction.label}
        </DropdownMenu.Item>
      {/if}
    {/if}
  </DropdownMenu.Content>
</DropdownMenu.Root>
