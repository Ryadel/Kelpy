import type { Access, Approval, Modes } from './api';

/** A new conversation's modes, before the user changes them. */
export const DEFAULT_MODES: Modes = { explanations: false, access: 'read_only', approval: 'ask' };

export const ACCESS_OPTIONS: { value: Access; name: string; description: string; disabled?: boolean }[] = [
  { value: 'none', name: 'None', description: 'Kelpy only answers. It reads nothing on this system.' },
  {
    value: 'read_only',
    name: 'Read-only',
    description:
      'Kelpy may look at this system: services, packages, ports, disks, files. It changes nothing, and each command it writes waits for your approval. What it reads is sent to the AI service.',
  },
  {
    value: 'full',
    name: 'Full',
    description:
      'Kelpy may also change the system. Its commands wait for your approval, but in Auto. They run as the account that started Kelpy.',
  },
];

export const APPROVAL_OPTIONS: { value: Approval; name: string; description: string }[] = [
  { value: 'ask', name: 'Ask', description: 'Every action waits for your approval.' },
  {
    value: 'plan',
    name: 'Plan',
    description:
      'Looking needs no approval. Kelpy proposes its changes as one plan, which you approve once; a change outside it waits for approval.',
  },
  {
    value: 'auto',
    name: 'Auto',
    description:
      'Looking, and the changes Kelpy checks and can put back, need no approval. With Full access, so do the commands Kelpy writes, but those that look like they could lose data, lock you out or stop the system: recognized by their text, which is no guarantee. Those, and changes that could lock you out or lose data, still wait for it.',
  },
];
