/**
 * The list of conversations, kept from copies that arrive out of order: an event and an HTTP answer about the same
 * change may cross. Each copy carries the conversation's revision, raised by every change in the database's own
 * order, so an older copy never replaces a newer one.
 */
import type { Session } from './api';

/** Whether `incoming` may replace `known`, the copy of the same conversation already listed. */
export function isNewer(incoming: Session, known: Session | undefined): boolean {
  return known === undefined || incoming.revision >= known.revision;
}

/** `sessions` with `incoming` in it, unless the copy listed is newer; most recent first. */
export function kept(sessions: Session[], incoming: Session): Session[] {
  const known = sessions.find((session) => session.id === incoming.id);
  if (!isNewer(incoming, known)) {
    return sessions;
  }
  return ordered([incoming, ...sessions.filter((session) => session.id !== incoming.id)]);
}

/** The conversations `fetched` lists, each as `listed` has it when that copy is newer; most recent first. */
export function merged(listed: Session[], fetched: Session[]): Session[] {
  return ordered(
    fetched.map((session) => {
      const known = listed.find((other) => other.id === session.id);
      return known && !isNewer(session, known) ? known : session;
    }),
  );
}

function ordered(sessions: Session[]): Session[] {
  return sessions.sort((a, b) => b.updated_at - a.updated_at || b.id - a.id);
}
