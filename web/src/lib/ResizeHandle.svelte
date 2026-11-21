<script lang="ts">
  /**
   * The inner edge of a side panel, which drags its width: the right edge of a panel on the left, the left edge of one
   * on the right. A line shows on hover; a double click gives back the usual width; the arrow keys move it when the
   * edge has the focus.
   */
  import { cn } from '$lib/utils';
  import { panelWidth, startWidth, type Panel } from './panels';

  interface Props {
    /** What it resizes, such as "the conversations". */
    label: string;
    panel: Panel;
    /** Which edge of the panel it is. */
    edge: 'left' | 'right';
    width: number;
    onResize: (width: number) => void;
    class?: string;
  }

  let { label, panel, edge, width, onResize, class: className }: Props = $props();

  /** Where the drag started: the pointer's position and the width then. */
  let drag = $state<{ x: number; width: number } | null>(null);
  let edgeElement = $state<HTMLDivElement | null>(null);

  /** The width the panel has on screen, which the window may hold below the width kept. */
  function current(): number {
    return startWidth(width, edgeElement?.parentElement?.getBoundingClientRect().width);
  }

  const STEP = 16;
  /** A pointer moving right widens a panel on the left and narrows one on the right. */
  const sign = $derived(edge === 'right' ? 1 : -1);

  function resize(to: number) {
    onResize(panelWidth(panel, to));
  }

  function start(event: PointerEvent) {
    if (event.button !== 0) return;
    // No text selection while dragging; the capture keeps the moves coming outside the edge.
    event.preventDefault();
    if (event.currentTarget instanceof HTMLElement) event.currentTarget.setPointerCapture(event.pointerId);
    drag = { x: event.clientX, width: current() };
  }

  function move(event: PointerEvent) {
    if (drag) resize(drag.width + sign * (event.clientX - drag.x));
  }

  function key(event: KeyboardEvent) {
    const change = { ArrowLeft: -STEP, ArrowRight: STEP }[event.key];
    if (change === undefined) return;
    event.preventDefault();
    resize(current() + sign * change);
  }
</script>

<!-- A focusable separator is the ARIA window splitter, which the checker counts as non-interactive. -->
<!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions -->
<div
  bind:this={edgeElement}
  role="separator"
  aria-orientation="vertical"
  aria-label={label}
  aria-valuenow={width}
  aria-valuemin={panel.min}
  aria-valuemax={panel.max}
  tabindex="0"
  title="Drag to resize · double-click for the usual width"
  class={cn(
    'group absolute inset-y-0 z-10 flex w-3 cursor-col-resize touch-none justify-center outline-none',
    edge === 'right' ? '-right-1.5' : '-left-1.5',
    className,
  )}
  onpointerdown={start}
  onpointermove={move}
  onpointerup={() => (drag = null)}
  onpointercancel={() => (drag = null)}
  ondblclick={() => onResize(panel.initial)}
  onkeydown={key}
>
  <span
    class={cn(
      'h-full w-0.5 transition-colors group-hover:bg-ring/50 group-focus-visible:bg-ring',
      drag && 'bg-ring group-hover:bg-ring',
    )}
  ></span>
</div>
