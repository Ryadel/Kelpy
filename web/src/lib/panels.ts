/**
 * The widths of the side panels, which the administrator drags to taste. Kept in this browser only, like the theme.
 */

export interface Panel {
  /** Where this browser keeps the width. */
  key: string;
  initial: number;
  min: number;
  max: number;
}

export const SIDEBAR: Panel = { key: 'kelpy.sidebar-width', initial: 260, min: 200, max: 480 };
export const COMMAND_LOG: Panel = { key: 'kelpy.command-log-width', initial: 380, min: 300, max: 760 };

/** `width` rounded and held within the panel's limits. */
export function panelWidth(panel: Panel, width: number): number {
  return Math.min(panel.max, Math.max(panel.min, Math.round(width)));
}

/**
 * The width a drag or a key starts from: the one shown, when the window holds the panel narrower than the width
 * kept, so that the edge moves at once instead of after a dead stretch.
 */
export function startWidth(width: number, shown: number | undefined): number {
  return shown !== undefined && Number.isFinite(shown) && shown > 0 && shown < width ? Math.round(shown) : width;
}

/** The width kept, or the initial one when there is none or a value that is not a number. */
export function storedWidth(panel: Panel, stored: string | null): number {
  const width = Number(stored);
  return stored === null || !Number.isFinite(width) ? panel.initial : panelWidth(panel, width);
}

export function loadWidth(panel: Panel): number {
  return storedWidth(panel, localStorage.getItem(panel.key));
}

/** Keeps `width` in this browser; the initial width is kept as no value. */
export function saveWidth(panel: Panel, width: number) {
  if (width === panel.initial) {
    localStorage.removeItem(panel.key);
  } else {
    localStorage.setItem(panel.key, String(width));
  }
}
