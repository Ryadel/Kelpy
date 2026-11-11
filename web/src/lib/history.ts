/**
 * The messages already sent in a conversation, browsed from the message box as a shell browses its commands: back
 * from the latest, and forward again to the draft that was being written.
 */
export class PromptHistory {
  /** How far back the box is: 0 is the draft, 1 the latest message sent. */
  #position = 0;
  #draft = '';

  /** `sent` gives the messages sent so far, oldest first. */
  constructor(private readonly sent: () => readonly string[]) {}

  get browsing(): boolean {
    return this.#position > 0;
  }

  /** The message one further back, keeping `current` as the draft when leaving it; null at the oldest. */
  back(current: string): string | null {
    const sent = this.sent();
    if (this.#position >= sent.length) {
      return null;
    }
    if (this.#position === 0) {
      this.#draft = current;
    }
    this.#position += 1;
    return sent[sent.length - this.#position];
  }

  /** The message one further forward, the draft after the latest; null when not browsing. */
  forward(): string | null {
    if (this.#position === 0) {
      return null;
    }
    this.#position -= 1;
    const sent = this.sent();
    return this.#position === 0 ? this.#draft : sent[sent.length - this.#position];
  }

  /** Typing, sending or another conversation ends the browsing: what the box holds is the draft. */
  reset() {
    this.#position = 0;
    this.#draft = '';
  }
}

/** What a conversation's history offers: its own messages, oldest first, a message repeated at once offered once. */
export function sentMessages(messages: readonly { role: string; content: string }[]): string[] {
  const sent: string[] = [];
  for (const message of messages) {
    if (message.role === 'user' && message.content.trim() !== '' && message.content !== sent.at(-1)) {
      sent.push(message.content);
    }
  }
  return sent;
}

/** Whether the caret, at `at` in `text`, is on its first line, where Up leaves the box to the history. */
export function onFirstLine(text: string, at: number): boolean {
  return !text.slice(0, at).includes('\n');
}

/** Whether the caret, at `at` in `text`, is on its last line, where Down goes on in the history. */
export function onLastLine(text: string, at: number): boolean {
  return !text.slice(at).includes('\n');
}
