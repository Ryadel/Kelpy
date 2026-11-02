<script lang="ts">
  import { elapsed } from './actions';

  interface Props {
    /** When it started, Unix time in seconds. */
    since: number;
  }

  let { since }: Props = $props();

  let now = $state(Date.now());

  $effect(() => {
    const timer = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(timer);
  });
</script>

<time>{elapsed(Math.floor(now / 1000) - since)}</time>
