<script lang="ts">
  import CheckIcon from '@lucide/svelte/icons/check';
  import { randomQuote } from './quotes';

  /** What Kelpy does, shown beside the first choice; a phone has no room for it. */
  const FACTS = [
    'Explains each command before it runs',
    'Asks before each change, by default',
    'Backs up the files it edits',
  ];
  const quote = randomQuote();
</script>

<!-- The first run's welcome: beside the first question, on a band above it on a phone. -->
<div class="welcome-panel flex flex-col items-center border-b px-5 pt-7 pb-4 text-center md:border-r md:border-b-0 md:px-8 md:pt-9 md:pb-7">
  <img src="/brand/kelpy-lockup-light.svg" alt="Kelpy" class="w-28 md:w-[150px] dark:hidden" />
  <img src="/brand/kelpy-lockup-dark.svg" alt="Kelpy" class="hidden w-28 md:w-[150px] dark:block" />
  <p class="text-muted-foreground mt-3 text-sm md:mt-4">AI-powered system administration.</p>
  <ul class="mt-6 hidden gap-2.5 text-left text-[13.5px] leading-snug md:grid">
    {#each FACTS as fact (fact)}
      <li class="flex gap-2.5"><CheckIcon class="text-primary mt-0.5 size-4 shrink-0" />{fact}</li>
    {/each}
  </ul>
  <p class="text-muted-foreground mt-3 text-xs italic md:mt-auto md:pt-7">“{quote}”</p>
</div>
