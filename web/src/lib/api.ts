import { connection } from './connection.svelte';
import { distrust, forgetSession, identify, loadSession, saveSession, type StoredSession } from './session';

/** What the runtime reports about itself through `/api/status`. */
export interface RuntimeStatus {
  version: string;
  pid: number;
  home: string;
  /** Unix time, in seconds. */
  started_at: number;
  /** The account Kelpy and its actions run as, such as `jsmith (not elevated)`. */
  account: string;
  /** Without administrator rights, what fails and how to have them. */
  account_limits: string | null;
}

/** What Kelpy may do on the system: only answer, look, or also change it. */
export type Access = 'none' | 'read_only' | 'full';
/** When Kelpy asks before acting. */
export type Approval = 'ask' | 'plan' | 'auto';

/** How a conversation's answers are written and what they may do. */
export interface Modes {
  /** Answers explain each step in plain words, instead of staying concise. */
  explanations: boolean;
  access: Access;
  approval: Approval;
}

export interface Session extends Modes {
  id: number;
  title: string;
  created_at: number;
  updated_at: number;
  /** Raised by every change of the conversation: of two copies, the higher is the newer. */
  revision: number;
}

export type ActionState = 'waiting' | 'running' | 'done' | 'failed' | 'denied' | 'uncertain';

/**
 * What Kelpy did on the system, or was asked to: an entry of the Command log. A call of the model in an answer, or
 * an operation the administrator asked for, such as putting a file back.
 */
export interface Action {
  id: number;
  session_id: number;
  /** The answer whose model called it; null for an operation the administrator asked for. */
  message_id: number | null;
  /** The model's step in the answer: the actions of one step were asked for together. */
  step: number;
  /** Where in the answer's text it was asked for, in UTF-8 bytes. */
  position: number;
  tool: string;
  /** Why the model asked for it. */
  reason: string;
  /** What it does, in one line. */
  summary: string;
  /** The commands it runs, one per line; null for an action that is not a command. */
  command: string | null;
  state: ActionState;
  exit_code: number | null;
  output: string;
  error: string | null;
  created_at: number;
  duration_ms: number | null;
  /** When it started running, Unix time in seconds. */
  started_at: number | null;
  /** The change it makes to the system; null for an action that does not change it. */
  change: Change | null;
  /** What a change is doing while it runs, such as "writing the file". */
  phase: string | null;
  /** The plan it proposes, for a plan. */
  plan: Plan | null;
  /** The plan, and the step of it, this action carries out. */
  plan_step: { plan_id: number; step: number } | null;
}

/** A change to the system, as its card shows it before it runs. */
export interface Change {
  /** What changes: a file's unified diff, the packages installed (+) and removed (-), a service's state. */
  diff: string;
  /** What Kelpy checks and how the change is undone, one line each. */
  notes: string[];
  /** Why it waits for approval in every Approval mode, if it does. */
  floor: string | null;
  /** The file it changes. */
  path?: string;
  /** Where the version of the file before the change was saved, once it ran. */
  backup?: string;
}

/** Changes the model proposes, approved together. */
export interface Plan {
  goal: string;
  steps: PlanStep[];
}

export interface PlanStep {
  tool: string;
  summary: string;
  /** The commands it runs, or the lines of the file it changes. */
  detail: string | null;
  /** Why it would ask outside the plan: approved with it, and marked. */
  floor: string | null;
}

/** What an operation of the administrator would do, or did. */
export type Operated =
  | { outcome: 'preview'; summary: string; change: Change }
  | { outcome: 'done'; action: Action };

/** A difference between what Kelpy left on the server and what is there: changed outside Kelpy. */
export interface Difference {
  id: number;
  kind: 'file' | 'service' | 'package';
  subject: string;
  expected: string;
  found: string;
  /** For a file whose content may be shown, what changed. */
  diff: string | null;
  found_at: number;
  recorded_at: number;
  /** Kelpy kept the file's content, and can put it back. */
  can_put_back: boolean;
}

export type MessageStatus = 'complete' | 'streaming' | 'interrupted' | 'failed' | 'refused';

export interface Message {
  id: number;
  session_id: number;
  role: 'user' | 'assistant';
  content: string;
  status: MessageStatus;
  model: string | null;
  created_at: number;
  /** For an answer, what it did on the system, in order. */
  actions: Action[];
}

export interface Conversation {
  messages: Message[];
  /** The operations the administrator asked for: in the Command log, not in answers. */
  administrator_actions: Action[];
  /** An answer is being written: it keeps going on the server even with no page open. */
  running: boolean;
  /** The last message summarised for the model: the model reads a summary of it and those before. */
  summarised_through: number | null;
}

export type ProviderKind = 'anthropic' | 'chatgpt' | 'openai' | 'ollama' | 'openai-compatible';

export interface ProviderConfig {
  kind: ProviderKind;
  model: string;
  base_url?: string;
  /** The service chosen in the setup, such as `groq`. */
  service?: string;
}

/** How a service is paid for: a ChatGPT plan, a free tier, an API key, or a local model. */
export type Way = 'plan' | 'free' | 'key' | 'local';

export interface Service {
  id: string;
  name: string;
  way: Way;
  kind: ProviderKind;
  /** The API address, when the kind has no default or the service uses another one. */
  base_url: string | null;
  description: string;
  key_page: string | null;
  steps: string[];
  /** Suggested models, the first one preselected. */
  models: string[];
}

export interface KindInfo {
  id: ProviderKind;
  default_model: string | null;
  default_base_url: string | null;
  key_variable: string | null;
  /** Whether an API key can be entered. */
  accepts_key: boolean;
  requires_key: boolean;
  /** The credential comes from signing in (a ChatGPT plan) instead of an API key. */
  signs_in: boolean;
  key_in_environment: boolean;
}

export interface ProviderInfo {
  configured: ProviderConfig | null;
  /** Where the key in use comes from; `null` when no provider is configured. */
  key: 'session' | 'environment' | 'missing' | null;
  /** What answers still need: sign-ins and keys entered here last only until Kelpy stops. */
  needs: 'sign_in' | 'key' | null;
  kinds: KindInfo[];
  /** The options of the first question, in order. */
  ways: { id: Way; name: string; description: string }[];
  services: Service[];
  /** The ways to sign in with a ChatGPT plan, in order. */
  sign_in_methods: SignInMethod[];
}

/** How a ChatGPT session is signed in: as the Codex CLI, or as an agent of the account. */
export type SignInMethodId = 'codex' | 'agent';

export interface SignInMethod {
  id: SignInMethodId;
  name: string;
  pros: string;
  cons: string;
  /** When to choose it. */
  recommended: string;
}

export interface ProviderUpdate extends ProviderConfig {
  /** Omitted keeps the key entered earlier; an empty string forgets it. */
  api_key?: string;
}

/** The service of a setup under way, whose models to list. */
export interface ModelQuery {
  kind: ProviderKind;
  service: string;
  base_url?: string;
  /** The key being entered; without it, the key in use for this endpoint or the environment's. */
  api_key?: string;
}

/** The models a setup offers: the recommended ones first, then those the service lists. */
export interface ModelList {
  models: { id: string; recommended: boolean }[];
  /** Why the service's own list is missing. */
  error: string | null;
}

export interface ChatGptAccount {
  sub: string;
  email: string | null;
}

export interface ChatGptStatus {
  /** The account signed in for this session of Kelpy. */
  signed_in: ChatGptAccount | null;
  /** How the session was signed in. */
  method: SignInMethodId | null;
  /** The session stays signed in on this computer across restarts, until a sign-out. */
  kept: boolean;
  /** The email of the account Kelpy is registered for as an agent. */
  registered: string | null;
}

/** An event of a sign-in with a code: the code to approve first, then the outcome. */
export type CodeEvent =
  | { type: 'code'; verification_url: string; user_code: string; expires_in: number; qr: string }
  | { type: 'signed_in'; account: ChatGptAccount }
  | { type: 'failed'; code: string; error: string };

export interface ChatGptModel {
  slug: string;
  display_name: string;
}

/** What OpenAI brings back to the redirect page. */
export interface ChatGptCallback {
  code: string | null;
  state: string | null;
  client_id: string | null;
  error: string | null;
}

export interface LocalModel {
  name: string;
  /** Bytes on disk. */
  size: number;
  /** Runs on ollama.com, not on Ollama's computer: prompts leave the network. */
  cloud: boolean;
}

export interface Suggestion {
  name: string;
  /** Bytes to download. */
  size: number;
  /** Bytes of RAM the computer should have. */
  memory: number;
  description: string;
}

export interface Download {
  base_url: string;
  model: string;
  status: string;
  completed: number;
  total: number;
  done: boolean;
  error: string | null;
}

export interface OllamaStatus {
  base_url: string;
  running: boolean;
  error: string | null;
  /** Whether the `ollama` program is installed on the computer running Kelpy. */
  installed: boolean;
  models: LocalModel[];
  /** RAM of the computer running Kelpy, in bytes, if known. */
  memory: number | null;
  suggestions: Suggestion[];
  downloads: Download[];
}

export type ChatEvent =
  | { type: 'user'; message: Message }
  | { type: 'started'; message: Message }
  | { type: 'delta'; text: string }
  /** An action of the answer, new or changed. */
  | { type: 'action'; action: Action }
  | { type: 'done'; message: Message; error: string | null; summarised_through: number | null };

/** A change in Kelpy, made by this page, another one or another interface. */
export type RuntimeEvent =
  | { type: 'session_created'; session: Session }
  /** Its title, its explanations, or the time of its last message. */
  | { type: 'session_changed'; session: Session }
  | { type: 'session_deleted'; id: number }
  | AnswerEvent
  /** An operation the administrator asked for started or ended. */
  | { type: 'action_changed'; session_id: number; action: Action }
  /** Changes made outside Kelpy were found or settled: read them again. */
  | { type: 'differences_changed' }
  /** The provider, or what it needs to answer, changed: read it again. */
  | { type: 'provider_changed' }
  /** Some events were missed: read again everything shown. */
  | { type: 'resync' }
  /** Kelpy is stopping: the last event. */
  | { type: 'stopping' };

/** An answer started, after the user's message, or ended; the text in between comes from following the answer. */
export type AnswerEvent =
  | { type: 'answer_started'; session_id: number; user: Message; answer: Message }
  | {
      type: 'answer_ended';
      session_id: number;
      message: Message;
      error: string | null;
      summarised_through: number | null;
    };

/** The runtime did not answer: Kelpy stopped, or the connection to it dropped. */
export class Unreachable extends Error {
  constructor() {
    super('Kelpy cannot be reached');
  }
}

/** The request needs a session: sign in with the access code first. */
export class Unauthorized extends Error {
  constructor() {
    super('not signed in');
  }
}

/** An error reported by the runtime, with a stable `code` such as `not_configured` or `missing_key`. */
export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message);
  }
}

export type SignInResult =
  | { ok: true }
  | { ok: false; reason: 'wrong-code' }
  | { ok: false; reason: 'too-many-attempts'; retryAfter: number }
  | { ok: false; reason: 'error'; message: string };

async function request(path: string, init: RequestInit = {}): Promise<Response> {
  const headers = new Headers(init.headers);
  headers.set('Accept', 'application/json');
  if (init.body !== undefined) {
    headers.set('Content-Type', 'application/json');
  }
  const session = loadSession();
  if (session !== null) {
    // The session goes only to the Kelpy that issued it (see `identify`).
    const identity = await identify(session);
    if (identity === 'ended') {
      forgetSession(session);
      throw new Unauthorized();
    }
    if (identity !== 'kelpy') {
      connection.reachable = false;
      throw new Unreachable();
    }
    headers.set('Authorization', `Bearer ${session.session}`);
  }
  let response: Response;
  try {
    response = await fetch(path, { ...init, headers });
  } catch (reason: unknown) {
    if (init.signal?.aborted) {
      throw reason;
    }
    distrust();
    connection.reachable = false;
    throw new Unreachable();
  }
  connection.reachable = true;
  if (response.status === 401) {
    forgetSession(session);
    throw new Unauthorized();
  }
  if (!response.ok) {
    const body = (await response.json().catch(() => ({}))) as {
      code?: string;
      error?: string;
    };
    throw new ApiError(response.status, body.code ?? 'error', body.error ?? `request failed (${response.status})`);
  }
  return response;
}

async function json<T>(path: string, init?: RequestInit): Promise<T> {
  return (await (await request(path, init)).json()) as T;
}

export const fetchStatus = (signal?: AbortSignal) => json<RuntimeStatus>('/api/status', { signal });

export const fetchProvider = () => json<ProviderInfo>('/api/provider');

export const saveProvider = (update: ProviderUpdate) =>
  json<ProviderInfo>('/api/provider', {
    method: 'PUT',
    body: JSON.stringify(update),
  });

/** A POST, because the query may carry the key being entered. */
export const fetchModels = (query: ModelQuery) =>
  json<ModelList>('/api/provider/models', {
    method: 'POST',
    body: JSON.stringify(query),
  });

export const fetchChatGpt = () => json<ChatGptStatus>('/api/chatgpt');

/** Starts a sign-in with ChatGPT; returns OpenAI's page to open. With `keep`, it stays across restarts. */
export const startChatGptSignIn = (keep: boolean) =>
  json<{ url: string }>('/api/chatgpt/sign-in', {
    method: 'POST',
    body: JSON.stringify({ keep }),
  });

/**
 * Signs in as the Codex CLI, calling `onEvent` with the code to approve, then with the outcome. Resolves when the
 * sign-in ends; aborting `signal` abandons it. With `keep`, the sign-in stays across restarts.
 */
export async function signInWithCode(
  onEvent: (event: CodeEvent) => void,
  keep: boolean,
  signal?: AbortSignal,
): Promise<void> {
  const response = await request('/api/chatgpt/code', {
    method: 'POST',
    body: JSON.stringify({ keep }),
    headers: { Accept: 'text/event-stream' },
    signal,
  });
  await readEvents(response, onEvent);
}

/** The agent registration as the text of a file for another Kelpy. */
export const fetchAgentFile = () => json<{ text: string }>('/api/chatgpt/agent');

/** Uses another Kelpy's agent registration file; returns the email of its account. */
export const useAgentFile = (text: string) =>
  json<{ registered: string | null }>('/api/chatgpt/agent', {
    method: 'PUT',
    body: JSON.stringify({ text }),
  });

/** Forgets the agent registration: the next sign-in as an agent registers again, with any account. */
export const forgetRegistration = () => json<ChatGptStatus>('/api/chatgpt/agent', { method: 'DELETE' });

export const finishChatGptSignIn = (callback: ChatGptCallback) =>
  json<ChatGptAccount>('/api/chatgpt/callback', {
    method: 'POST',
    body: JSON.stringify(callback),
  });

export const fetchChatGptModels = () => json<ChatGptModel[]>('/api/chatgpt/models');

/**
 * Ends the ChatGPT session. `revoked`: OpenAI confirmed the revocation; `forgotten`: the session kept on this computer,
 * if any, was deleted.
 */
export const signOutOfChatGpt = () =>
  json<{ revoked: boolean; forgotten: boolean }>('/api/chatgpt/sign-out', {
    method: 'POST',
    body: '{}',
  });

/** The page OpenAI sends the browser back to, with a fixed path. */
const CHATGPT_RETURN = '/auth/callback';
const CHATGPT_RETURN_KEY = 'kelpy.chatgpt-callback';

/**
 * Runs first on every page load. On OpenAI's return page, keeps the parameters for this tab and moves on to the
 * Web UI's own address, leaving no code in the address bar or the history. Returns whether the page is moving on.
 */
export function keepChatGptReturn(): boolean {
  if (window.location.pathname !== CHATGPT_RETURN) {
    return false;
  }
  const params = new URLSearchParams(window.location.search);
  const callback: ChatGptCallback = {
    code: params.get('code'),
    state: params.get('state'),
    client_id: params.get('client_id'),
    error: params.get('error'),
  };
  sessionStorage.setItem(CHATGPT_RETURN_KEY, JSON.stringify(callback));
  window.location.replace('/');
  return true;
}

/** The parameters kept by `keepChatGptReturn`, once. */
export function takeChatGptReturn(): ChatGptCallback | null {
  const kept = sessionStorage.getItem(CHATGPT_RETURN_KEY);
  sessionStorage.removeItem(CHATGPT_RETURN_KEY);
  return kept === null ? null : (JSON.parse(kept) as ChatGptCallback);
}

export const fetchOllama = (baseUrl: string) =>
  json<OllamaStatus>(`/api/ollama?${new URLSearchParams(baseUrl ? { base_url: baseUrl } : {})}`);

/**
 * Downloads a model into Ollama, or follows its download, calling `onProgress` as it goes. Resolves when the download
 * ends. Aborting `signal` only stops following: the download goes on.
 */
/** How Ollama would be installed on this computer, to approve. */
export interface OllamaInstaller {
  command: string;
  notes: string[];
}

/** An installation's progress: the installer's output so far. */
export interface OllamaInstallation {
  command: string;
  output: string;
  done: boolean;
  error: string | null;
}

export const fetchOllamaInstaller = () =>
  json<OllamaInstaller>('/api/ollama/install', { method: 'POST', body: JSON.stringify({ preview: true }) });

/** Installs Ollama, or follows the installation under way, until it ends. */
export async function installOllama(
  onProgress: (installation: OllamaInstallation) => void,
  signal?: AbortSignal,
): Promise<void> {
  const response = await request('/api/ollama/install', {
    method: 'POST',
    body: '{}',
    headers: { Accept: 'text/event-stream' },
    signal,
  });
  await readEvents(response, onProgress);
}

export async function downloadModel(
  baseUrl: string,
  model: string,
  onProgress: (download: Download) => void,
  signal?: AbortSignal,
): Promise<void> {
  const response = await request('/api/ollama/download', {
    method: 'POST',
    body: JSON.stringify({ base_url: baseUrl, model }),
    headers: { Accept: 'text/event-stream' },
    signal,
  });
  await readEvents(response, onProgress);
}

export const fetchSessions = () => json<Session[]>('/api/sessions');

export const createSession = (modes: Modes) =>
  json<Session>('/api/sessions', { method: 'POST', body: JSON.stringify(modes) });

export const renameSession = (id: number, title: string) =>
  json<Session>(`/api/sessions/${id}`, {
    method: 'PATCH',
    body: JSON.stringify({ title }),
  });

/** Changes some of a conversation's modes. */
export const setModes = (id: number, modes: Partial<Modes>) =>
  json<Session>(`/api/sessions/${id}`, {
    method: 'PATCH',
    body: JSON.stringify(modes),
  });

/**
 * Approves or denies an action waiting for a decision. Approving can switch the conversation to an Approval mode in
 * which the action would not have asked.
 */
export async function decideAction(
  sessionId: number,
  actionId: number,
  decision: 'approve' | 'deny',
  switchTo?: Approval,
): Promise<void> {
  await request(`/api/sessions/${sessionId}/actions/${actionId}`, {
    method: 'POST',
    body: JSON.stringify({ decision, switch_to: switchTo }),
  });
}

/** Stops an action running. The answer goes on, and the model is told that the administrator stopped it. */
export async function stopAction(sessionId: number, actionId: number): Promise<void> {
  await request(`/api/sessions/${sessionId}/actions/${actionId}/stop`, { method: 'POST' });
}

/** Puts a file back from the backup of a change: with `preview`, only shows what would change. */
export const restoreAction = (sessionId: number, actionId: number, preview: boolean) =>
  json<Operated>(`/api/sessions/${sessionId}/actions/${actionId}/restore`, {
    method: 'POST',
    body: JSON.stringify({ preview }),
  });

export const fetchDifferences = () => json<Difference[]>('/api/differences');

/** Checks the server now for changes made outside Kelpy. */
export const checkDifferences = () => json<Difference[]>('/api/differences/check', { method: 'POST' });

/** Keeps what was found: it is expected from now on. */
export async function acceptDifference(id: number): Promise<void> {
  await request(`/api/differences/${id}/accept`, { method: 'POST' });
}

/** Puts Kelpy's version of a file back, as an operation in the conversation: with `preview`, only shows it. */
export const putBack = (sessionId: number, id: number, preview: boolean) =>
  json<Operated>(`/api/sessions/${sessionId}/differences/${id}/put-back`, {
    method: 'POST',
    body: JSON.stringify({ preview }),
  });

/** The handover report of a conversation, in Markdown. */
export async function fetchReport(id: number): Promise<string> {
  return (await request(`/api/sessions/${id}/report`)).text();
}

/** The operations of a conversation that succeeded, as JSON text. */
export async function fetchExport(id: number): Promise<string> {
  return JSON.stringify(await json<unknown>(`/api/sessions/${id}/export`), null, 2);
}

export async function deleteSession(id: number): Promise<void> {
  await request(`/api/sessions/${id}`, { method: 'DELETE' });
}

export const fetchConversation = (id: number) => json<Conversation>(`/api/sessions/${id}/messages`);

export async function cancelAnswer(id: number): Promise<void> {
  await request(`/api/sessions/${id}/cancel`, { method: 'POST' }).catch((reason: unknown) => {
    // Already finished: nothing to stop.
    if (!(reason instanceof ApiError && reason.status === 404)) {
      throw reason;
    }
  });
}

/**
 * Sends a message and calls `onEvent` for each event of the answer, as it is written. Resolves when the stream
 * ends. Aborting `signal` only stops listening: the answer goes on and is saved on the server.
 */
export async function sendMessage(
  id: number,
  content: string,
  onEvent: (event: ChatEvent) => void,
  signal?: AbortSignal,
): Promise<void> {
  const response = await request(`/api/sessions/${id}/messages`, {
    method: 'POST',
    body: JSON.stringify({ content }),
    headers: { Accept: 'text/event-stream' },
    signal,
  });
  await readEvents(response, onEvent);
}

/**
 * Follows the answer being written in a conversation, from where it is: first `started` with the text so far, then
 * the rest. Resolves when the answer ends; fails with `not_running` when there is none.
 */
export async function followAnswer(
  id: number,
  onEvent: (event: ChatEvent) => void,
  signal?: AbortSignal,
): Promise<void> {
  const response = await request(`/api/sessions/${id}/answer`, {
    headers: { Accept: 'text/event-stream' },
    signal,
  });
  await readEvents(response, onEvent);
}

/**
 * Opens the stream of Kelpy's events. Resolves once Kelpy has subscribed this page: nothing that happens from then
 * on is missed. Aborting `signal` closes the stream.
 */
export const openEvents = (signal: AbortSignal) =>
  request('/api/events', { headers: { Accept: 'text/event-stream' }, signal });

/**
 * Reads a Server-Sent Events stream of JSON events until it ends. With `silence`, a stream quiet for longer, in
 * milliseconds, is taken as dropped and ends.
 */
export async function readEvents<T>(response: Response, onEvent: (event: T) => void, silence?: number): Promise<void> {
  if (!response.body) {
    return;
  }
  const reader = response.body.pipeThrough(new TextDecoderStream()).getReader();
  let quiet: ReturnType<typeof setTimeout> | undefined;
  let buffer = '';
  for (;;) {
    if (silence !== undefined) {
      // Cancelling makes the pending read return as at the stream's end.
      quiet = setTimeout(() => void reader.cancel(), silence);
    }
    let read: ReadableStreamReadResult<string>;
    try {
      read = await reader.read();
    } catch (reason: unknown) {
      if (reason instanceof DOMException && reason.name === 'AbortError') {
        throw reason;
      }
      connection.reachable = false;
      throw new Unreachable();
    } finally {
      clearTimeout(quiet);
    }
    const { value, done } = read;
    if (done) {
      return;
    }
    buffer += value.replace(/\r\n/g, '\n');
    let end: number;
    while ((end = buffer.indexOf('\n\n')) >= 0) {
      const block = buffer.slice(0, end);
      buffer = buffer.slice(end + 2);
      const data = block
        .split('\n')
        .filter((line) => line.startsWith('data:'))
        .map((line) => line.slice(5).trimStart())
        .join('\n');
      if (data) {
        onEvent(JSON.parse(data) as T);
      }
    }
  }
}

/** Exchanges the access code for a session, which every request sends from then on. */
export async function signIn(code: string): Promise<SignInResult> {
  const response = await fetch('/api/session', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ code }),
  });
  if (response.ok) {
    saveSession((await response.json()) as StoredSession);
    return { ok: true };
  }
  if (response.status === 401) {
    return { ok: false, reason: 'wrong-code' };
  }
  if (response.status === 429) {
    const retryAfter = Number(response.headers.get('Retry-After')) || 10;
    return { ok: false, reason: 'too-many-attempts', retryAfter };
  }
  return {
    ok: false,
    reason: 'error',
    message: `sign-in failed (${response.status})`,
  };
}

/** Asks Kelpy to stop: the runtime finishes what it can within its grace period, then exits. */
export async function stopKelpy(): Promise<void> {
  await request('/api/shutdown', { method: 'POST' });
}

/** Ends the session. This browser forgets it even when Kelpy cannot be reached. */
export async function signOut(): Promise<void> {
  const session = loadSession();
  try {
    await request('/api/session', { method: 'DELETE' });
  } catch (reason: unknown) {
    // Already ended, or Kelpy is not there to forget it: either way, nobody can use it once this browser forgets it.
    if (!(reason instanceof Unauthorized || reason instanceof Unreachable)) {
      throw reason;
    }
  } finally {
    forgetSession(session);
  }
}

/**
 * Takes the access code from a one-click link (`/#code=…`) and removes it from the address bar and the history
 * entry, so it is not left on screen or in the browser history.
 */
export function takeCodeFromLink(): string | null {
  const match = /^#code=([^&]*)/.exec(window.location.hash);
  if (!match) {
    return null;
  }
  history.replaceState(history.state, '', window.location.pathname + window.location.search);
  return decodeURIComponent(match[1]);
}

/** A size in bytes, in gigabytes with one decimal: `6.6 GB`. */
export function gigabytes(bytes: number): string {
  return `${(bytes / 1e9).toFixed(1)} GB`;
}

export function describe(reason: unknown): string {
  return reason instanceof Error ? reason.message : String(reason);
}
