import type { RuntimeStatus } from './api';

/** What the runtime reports about itself, once the page has it: shared by the parts of the page that show it. */
export const runtime = $state<{ status: RuntimeStatus | null }>({ status: null });
