import { expect, test } from 'bun:test';
import { failedForRights, needsAdministrator } from '../src/lib/rights';

test('the errors of a missing right are recognised, as Windows and Linux word them', () => {
  for (const error of [
    "Service 'World Wide Web Publishing Service (W3SVC)' cannot be stopped: Cannot open W3SVC service on computer '.'. Access is denied.",
    'The requested operation requires elevation.',
    'Windows lets only administrators change its features and IIS',
    'cp: cannot create regular file /etc/nginx/nginx.conf: Permission denied',
    'Failed to reload nginx.service: Interactive authentication required.',
    'E: Could not open lock file /var/lib/dpkg/lock-frontend - open (13: Permission denied)',
    'chown: changing ownership of /srv: Operation not permitted',
  ]) {
    expect(needsAdministrator(error)).toBe(true);
  }
  for (const error of ['exit 1', 'No such file or directory', null, undefined]) {
    expect(needsAdministrator(error)).toBe(false);
  }
});

test('a command that ran and ended badly for lack of rights counts, its output saying so', () => {
  const ran = (exit_code: number | null, output: string) => ({ state: 'done' as const, error: null, exit_code, output });
  expect(failedForRights(ran(1, 'Stop-Service : Service W3SVC cannot be stopped... Access is denied.'))).toBe(true);
  expect(failedForRights(ran(126, `${'listing\n'.repeat(2000)}sh: /usr/sbin/nginx: Permission denied`))).toBe(true);
  // Done without failing, or failing for another reason.
  expect(failedForRights(ran(0, 'find: /root: Permission denied'))).toBe(false);
  expect(failedForRights(ran(null, 'Permission denied'))).toBe(false);
  expect(failedForRights(ran(2, 'No such file or directory'))).toBe(false);
  // Failed before it ran: its error says why.
  expect(failedForRights({ state: 'failed', error: 'Access is denied.', exit_code: null, output: '' })).toBe(true);
  expect(failedForRights({ state: 'denied', error: 'Access is denied.', exit_code: null, output: '' })).toBe(false);
});
