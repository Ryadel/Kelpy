<script lang="ts">
  import MoonIcon from '@lucide/svelte/icons/moon';
  import SunIcon from '@lucide/svelte/icons/sun';
  import { Button } from '$lib/components/ui/button';

  /** Where the choice is kept; `theme.js` reads it before the page is drawn. */
  const KEY = 'kelpy.theme';

  let dark = $state(document.documentElement.classList.contains('dark'));
  const label = $derived(dark ? 'Light theme' : 'Dark theme');

  function toggle() {
    dark = !dark;
    document.documentElement.classList.toggle('dark', dark);
    localStorage.setItem(KEY, dark ? 'dark' : 'light');
  }
</script>

<Button variant="ghost" size="icon-lg" class="text-muted-foreground" onclick={toggle} aria-label={label} title={label}>
  {#if dark}<SunIcon class="size-[18px]" />{:else}<MoonIcon class="size-[18px]" />{/if}
</Button>
