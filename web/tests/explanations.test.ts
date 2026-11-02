import { describe, expect, test } from 'bun:test';
import { Lexer } from 'marked';
import { type Code, hasCallouts, isExplanation, joinExplanations } from '../src/lib/explanations';

const fence = '```';
const lex = (markdown: string) => joinExplanations(new Lexer({ gfm: true }).lex(markdown));
const block = (lang: string, text: string) => `${fence}${lang}\n${text}\n${fence}`;

describe('joinExplanations', () => {
  test("an explanation after a command's code block becomes its tab", () => {
    const tokens = lex(`Restart IIS:\n\n${block('powershell', 'iisreset')}\n\n${block('explanation', 'It **restarts** IIS.')}`);
    const code = tokens.find((token) => token.type === 'code') as Code;
    expect(code.text).toBe('iisreset');
    expect(code.explanation).toBe('It **restarts** IIS.');
    expect(tokens.some(isExplanation)).toBe(false);
  });

  test('an explanation after text, or a second one, stays a callout', () => {
    const tokens = lex(
      `A log:\n\n${block('explanation', 'A file.')}\n\n${block('bash', 'ls')}\n${block('explanation', 'Lists.')}\n${block('explanation', 'More.')}`,
    );
    expect(tokens.filter(isExplanation).map((token) => token.text)).toEqual(['A file.', 'More.']);
    expect((tokens.find((token) => token.type === 'code' && !isExplanation(token)) as Code).explanation).toBe('Lists.');
  });

  test('the links defined in the answer are kept', () => {
    const tokens = lex('See [the docs][d].\n\n[d]: https://example.com');
    expect(tokens.links.d.href).toBe('https://example.com');
  });
});

describe('hasCallouts', () => {
  test('only explanations of their own are callouts', () => {
    expect(hasCallouts(`${block('bash', 'ls')}\n\n${block('explanation', 'Lists.')}`)).toBe(false);
    expect(hasCallouts(`Text.\n\n${block('explanation', 'Why.')}`)).toBe(true);
    expect(hasCallouts('No explanations.')).toBe(false);
  });
});
