/**
 * Text written but not sent, per conversation, kept for this browser tab only: it survives a reload, or opening the
 * new link of a Kelpy started again on the same port, and is gone when the tab closes.
 */

/** A conversation's id, or `new` for one not created yet. */
type DraftOf = number | 'new';

const key = (of: DraftOf) => `kelpy-draft-${of}`;

export function loadDraft(of: DraftOf): string {
  return sessionStorage.getItem(key(of)) ?? '';
}

export function saveDraft(of: DraftOf, text: string) {
  if (text.trim() === '') {
    sessionStorage.removeItem(key(of));
  } else {
    sessionStorage.setItem(key(of), text);
  }
}
