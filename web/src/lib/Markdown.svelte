<script lang="ts" module>
  import { Marked } from 'marked';
  import { copyText } from './clipboard';
  import { type Code, joinExplanations } from './explanations';
  import { EXPLANATION } from './reveal';

  const escapeHtml = (text: string) =>
    text.replace(/[&<>"']/g, (char) => `&#${char.charCodeAt(0)};`);

  const copyIcon =
    '<svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect width="14" height="14" x="8" y="8" rx="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/></svg>';

  const bulbIcon =
    '<svg xmlns="http://www.w3.org/2000/svg" width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M15 14c.2-1 .7-1.7 1.5-2.5 1-.9 1.5-2.2 1.5-3.5A6 6 0 0 0 6 8c0 1 .2 2.2 1.5 3.5.7.7 1.3 1.5 1.5 2.5"/><path d="M9 18h6"/><path d="M10 22h4"/></svg>';

  const copyButton = (what: string) =>
    `<button type="button" data-copy title="Copies ${what}">${copyIcon}<span>Copy</span></button>`;

  /**
   * Code blocks get a header with their language and a Copy button, handled by `copy` below. The model's explanations
   * (see `explanations.ts`) are a tab of the code block they follow, or else a callout; either is copied as written.
   */
  const markdown: Marked = new Marked({
    gfm: true,
    breaks: false,
    hooks: { processAllTokens: joinExplanations },
    renderer: {
      code(token): string {
        const { text, lang, explanation } = token as Code;
        const rendered = (source: string) => markdown.parse(source, { async: false });
        if (lang?.trim() === EXPLANATION) {
          return (
            `<details class="explanation" open data-source="${escapeHtml(text)}"><summary>` +
            `<span class="explanation-title">${bulbIcon}Explanation</span>${copyButton('the explanation')}</summary>` +
            `<div class="explanation-body">${rendered(text)}</div></details>`
          );
        }
        const language = escapeHtml(lang?.match(/^\S+/)?.[0] ?? 'text');
        const code = `<pre><code>${escapeHtml(text)}</code></pre>`;
        if (explanation === undefined) {
          return (
            `<div class="code-block"><div class="code-head"><span>${language}</span>${copyButton('the code')}</div>` +
            `${code}</div>`
          );
        }
        return (
          `<div class="code-block explained" data-source="${escapeHtml(explanation)}"><div class="code-head">` +
          `<span class="tabs"><button type="button" class="tab" data-tab="code" aria-pressed="true">${language}</button>` +
          `<button type="button" class="tab" data-tab="explanation" aria-pressed="false">${bulbIcon}Explanation</button>` +
          `</span>${copyButton('what is shown')}</div>${code}` +
          `<div class="tab-explanation">${rendered(explanation)}</div></div>`
        );
      },
    },
  });
</script>

<script lang="ts">
  import DOMPurify from 'dompurify';

  /** `explanationsOpen` opens or closes every explanation panel; each can still be opened or closed alone. */
  let { text, explanationsOpen = true }: { text: string; explanationsOpen?: boolean } = $props();

  let root = $state<HTMLElement | null>(null);

  /** The code blocks, by their order, showing their explanation tab: kept as the text is rendered again. */
  let explaining = $state(new Set<number>());

  // Model output is untrusted: rendered to HTML, then sanitized before it reaches the page.
  const html = $derived(DOMPurify.sanitize(markdown.parse(text, { async: false })));

  // After each rendering too: new callouts follow the choice made for the answer, tabs stay where they were.
  $effect(() => {
    void html;
    const open = explanationsOpen;
    root?.querySelectorAll('details.explanation').forEach((panel) => {
      (panel as HTMLDetailsElement).open = open;
    });
    root?.querySelectorAll('.code-block.explained').forEach((block, index) => showTab(block, explaining.has(index)));
  });

  /** Shows the code block's explanation tab, or its code. */
  function showTab(block: Element, explanation: boolean) {
    block.classList.toggle('showing-explanation', explanation);
    block.querySelectorAll('[data-tab]').forEach((tab) => {
      tab.setAttribute('aria-pressed', String((tab.getAttribute('data-tab') === 'explanation') === explanation));
    });
  }

  /** The clicks in the rendered HTML: a code block's tabs, and the Copy buttons. */
  function click(event: MouseEvent) {
    const tab = (event.target as Element).closest('[data-tab]');
    const block = tab?.closest('.code-block.explained');
    if (tab && block && root) {
      const index = [...root.querySelectorAll('.code-block.explained')].indexOf(block);
      const explanation = tab.getAttribute('data-tab') === 'explanation';
      const next = new Set(explaining);
      if (explanation) {
        next.add(index);
      } else {
        next.delete(index);
      }
      explaining = next;
      showTab(block, explanation);
      return;
    }
    void copy(event);
  }

  /** Copies a code block, the tab it shows, or a callout, from its Copy button. */
  async function copy(event: MouseEvent) {
    const button = (event.target as Element).closest('[data-copy]');
    const block = button?.closest('.code-block, .explanation');
    const label = button?.querySelector('span');
    if (!block || !label) {
      return;
    }
    // A Copy in a callout's title must not close it.
    event.preventDefault();
    const code = block.querySelector('code');
    const explanation = block.matches('.explanation, .showing-explanation');
    const source = explanation ? (block as HTMLElement).dataset.source : code?.textContent;
    if (await copyText(source ?? '')) {
      label.textContent = 'Copied';
      setTimeout(() => (label.textContent = 'Copy'), 2000);
    } else if (code) {
      // A page on plain HTTP from another host has no clipboard: the code is selected, to copy by hand.
      getSelection()?.selectAllChildren(code);
    }
  }
</script>

<!-- Keyboard users reach the Copy buttons themselves; their clicks bubble up to here. -->
<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
<div class="markdown" onclick={click} bind:this={root}>
  <!-- eslint-disable-next-line svelte/no-at-html-tags -->
  {@html html}
</div>

<style>
  .markdown {
    line-height: 1.7;
    overflow-wrap: anywhere;
  }
  .markdown :global(p) {
    margin: 0 0 0.75em;
  }
  .markdown > :global(:last-child) {
    margin-bottom: 0;
  }
  .markdown :global(ul),
  .markdown :global(ol) {
    margin: 0.25em 0 0.9em;
    padding-left: 1.4em;
  }
  .markdown :global(ul) {
    list-style: disc;
  }
  .markdown :global(ol) {
    list-style: decimal;
  }
  .markdown :global(li) {
    margin: 0.4em 0;
    padding-left: 0.2em;
  }
  .markdown :global(li > p) {
    margin-bottom: 0.5em;
  }
  .markdown :global(h1),
  .markdown :global(h2),
  .markdown :global(h3) {
    margin: 1.2em 0 0.5em;
    font-weight: 600;
  }
  .markdown > :global(:first-child) {
    margin-top: 0;
  }
  .markdown :global(a) {
    color: var(--primary);
    text-decoration: underline;
  }
  .markdown :global(:not(pre) > code) {
    font-family: var(--font-mono);
    font-size: 0.86em;
    padding: 0.12em 0.38em;
    border-radius: 0.35em;
    background: var(--muted);
  }
  .markdown :global(.code-block) {
    margin: 0.6em 0 0.9em;
    border: 1px solid var(--border);
    border-radius: 0.85rem;
    overflow: hidden;
    background: var(--code);
  }
  .markdown :global(.code-head) {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 0.4rem 0.6rem 0.4rem 0.9rem;
    background: var(--code-head);
    font-size: 12px;
    line-height: 1.5;
    color: var(--muted-foreground);
  }
  .markdown :global([data-copy]) {
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    padding: 0.15rem 0.45rem;
    border-radius: 0.4rem;
    cursor: pointer;
  }
  .markdown :global([data-copy]:hover) {
    color: var(--foreground);
    background: var(--muted);
  }
  .markdown :global(pre) {
    margin: 0;
    padding: 0.85rem 1rem;
    overflow-x: auto;
    font-family: var(--font-mono);
    font-size: 13px;
    line-height: 1.6;
    color: var(--code-foreground);
    overflow-wrap: normal;
  }
  .markdown :global(table) {
    border-collapse: collapse;
    margin: 0.75em 0;
  }
  .markdown :global(th),
  .markdown :global(td) {
    border: 1px solid var(--border);
    padding: 0.25rem 0.5rem;
  }
  .markdown :global(.explanation) {
    margin: 0.2em 0 1em;
    padding: 0.5rem 0.9rem 0.6rem 1rem;
    border-left: 4px solid var(--primary);
    border-radius: 0 0.6rem 0.6rem 0;
    background: color-mix(in oklab, var(--primary) 10%, var(--background));
    font-size: 13px;
    line-height: 1.6;
  }
  .markdown :global(.explanation > summary) {
    display: flex;
    align-items: center;
    justify-content: space-between;
    cursor: pointer;
    list-style: none;
  }
  .markdown :global(.explanation > summary::-webkit-details-marker) {
    display: none;
  }
  .markdown :global(.explanation-title) {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--primary);
  }
  .markdown :global(.explanation:not([open]) .explanation-title::after) {
    content: '…';
  }
  .markdown :global(.explanation-body) {
    margin-top: 0.2rem;
    color: color-mix(in oklab, var(--foreground) 82%, var(--muted-foreground));
  }
  .markdown :global(.explanation-body > :last-child),
  .markdown :global(.tab-explanation > :last-child) {
    margin-bottom: 0;
  }
  .markdown :global(.tabs) {
    display: inline-flex;
    gap: 0.2rem;
    margin: -0.15rem 0 -0.15rem -0.5rem;
  }
  .markdown :global(.tab) {
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    padding: 0.1rem 0.6rem;
    border-radius: 0.45rem;
    cursor: pointer;
  }
  .markdown :global(.tab:hover) {
    color: var(--foreground);
  }
  .markdown :global(.tab[aria-pressed='true']) {
    background: var(--code);
    color: var(--foreground);
  }
  .markdown :global(.tab[data-tab='explanation']) {
    color: var(--primary);
  }
  .markdown :global(.tab[data-tab='explanation'][aria-pressed='true']) {
    background: color-mix(in oklab, var(--primary) 16%, var(--code));
  }
  .markdown :global(.tab-explanation) {
    display: none;
    padding: 0.75rem 1rem;
    font-size: 13px;
    line-height: 1.6;
    background: color-mix(in oklab, var(--primary) 10%, var(--code));
    color: color-mix(in oklab, var(--foreground) 85%, var(--muted-foreground));
  }
  .markdown :global(.showing-explanation pre) {
    display: none;
  }
  .markdown :global(.showing-explanation .tab-explanation) {
    display: block;
  }
  .markdown :global(blockquote) {
    border-left: 3px solid var(--border);
    padding-left: 0.75rem;
    color: var(--muted-foreground);
  }
</style>
