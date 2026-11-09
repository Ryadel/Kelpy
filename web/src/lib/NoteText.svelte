<script lang="ts">
  import { codeSpans } from './actions';

  /** A note from Kelpy, its commands and paths between backticks shown as code. */
  let { text }: { text: string } = $props();
</script>

{#each codeSpans(text) as part, index (index)}{#if part.code}<code>{part.text}</code>{:else}{part.text}{/if}{/each}
