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
      'Kelpy may also change the system, with commands that each wait for your approval. They run as the account that started Kelpy.',
  },
];

export const APPROVAL_OPTIONS: { value: Approval; name: string; description: string }[] = [
  { value: 'ask', name: 'Ask', description: 'Every action waits for your approval.' },
  { value: 'plan', name: 'Plan', description: 'Looking needs no approval. Each command Kelpy writes still waits for it.' },
  { value: 'auto', name: 'Auto', description: 'Looking needs no approval. Each command Kelpy writes still waits for it.' },
];
