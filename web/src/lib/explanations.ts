/**
 * Where the model's explanations show. It writes one in an `explanation` block after each step: one that follows a
 * command's code block becomes a tab of that block, any other a callout of its own.
 */
import { Lexer, type Token, type Tokens, type TokensList } from 'marked';
import { EXPLANATION } from './reveal';

/** A code block, with the explanation that followed it when it has one. */
export type Code = Tokens.Code & { explanation?: string };

/** Whether `token` is an explanation block. */
export const isExplanation = (token: Token): token is Tokens.Code =>
  token.type === 'code' && (token as Tokens.Code).lang?.trim() === EXPLANATION;

/** `tokens` with each explanation that follows a command's code block joined to that block. */
export function joinExplanations<T extends Token[] | TokensList>(tokens: T): T {
  const joined: Token[] = [];
  for (const token of tokens) {
    let previous: Token | undefined;
    for (let index = joined.length - 1; index >= 0 && !previous; index--) {
      previous = joined[index].type === 'space' ? undefined : joined[index];
    }
    if (isExplanation(token) && previous?.type === 'code' && !isExplanation(previous)) {
      const code = previous as Code;
      if (code.explanation === undefined) {
        code.explanation = token.text;
        continue;
      }
    }
    joined.push(token);
  }
  return ('links' in tokens ? Object.assign(joined, { links: tokens.links }) : joined) as T;
}

/** Whether `markdown` has explanations shown as callouts, which the answer's toggle opens and closes. */
export function hasCallouts(markdown: string): boolean {
  return joinExplanations(new Lexer({ gfm: true }).lex(markdown)).some(isExplanation);
}
