import { describe, expect, test } from 'bun:test';
import { onFirstLine, onLastLine, PromptHistory, sentMessages } from '../src/lib/history';

describe('PromptHistory', () => {
  test('goes back from the latest message and forward to the draft', () => {
    const history = new PromptHistory(() => ['uptime?', 'df -h?']);
    expect(history.back('half wri')).toBe('df -h?');
    expect(history.back('df -h?')).toBe('uptime?');
    expect(history.back('uptime?')).toBeNull();
    expect(history.forward()).toBe('df -h?');
    expect(history.forward()).toBe('half wri');
    expect(history.browsing).toBe(false);
    expect(history.forward()).toBeNull();
  });

  test('typing ends the browsing: the next Up starts from the latest again', () => {
    const sent = ['one', 'two'];
    const history = new PromptHistory(() => sent);
    history.back('');
    history.back('two');
    history.reset();
    expect(history.back('one, edited')).toBe('two');
    expect(history.forward()).toBe('one, edited');
  });

  test('a conversation with nothing sent offers nothing', () => {
    const history = new PromptHistory(() => []);
    expect(history.back('draft')).toBeNull();
    expect(history.forward()).toBeNull();
  });
});

test('only the conversation’s own messages, a repeat offered once', () => {
  const messages = [
    { role: 'user', content: 'uptime?' },
    { role: 'assistant', content: 'Up 3 days.' },
    { role: 'user', content: 'uptime?' },
    { role: 'user', content: '  ' },
    { role: 'user', content: 'df -h?' },
  ];
  expect(sentMessages(messages)).toEqual(['uptime?', 'df -h?']);
});

test('Up and Down leave the box only from its first and last line', () => {
  const text = 'first\nsecond';
  expect(onFirstLine(text, 3)).toBe(true);
  expect(onFirstLine(text, 8)).toBe(false);
  expect(onLastLine(text, 8)).toBe(true);
  expect(onLastLine(text, 3)).toBe(false);
});
