/**
 * Whether this browser shows the lore while Kelpy thinks: a passage of folklore above the message box. On unless
 * turned off; the choice is kept in this browser only, like the theme.
 */

const KEY = 'kelpy.lore';

export const lore = $state({ on: localStorage.getItem(KEY) !== 'off' });

/** Turns the lore on or off in this browser. */
export function showLore(on: boolean) {
  lore.on = on;
  if (on) {
    localStorage.removeItem(KEY);
  } else {
    localStorage.setItem(KEY, 'off');
  }
}
