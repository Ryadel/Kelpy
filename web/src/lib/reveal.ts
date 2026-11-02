/**
 * Showing an answer as it is written: the text that arrives in bursts is revealed a few words at a time, and never
 * with a Markdown or HTML construct left open, which would show its signs until it closes.
 */

/** The language of the blocks in which the model writes its explanations, shown as panels. */
export const EXPLANATION = 'explanation';

/** How fast a burst of text shows: about a fifth of what waits in each frame of 16 ms, so most of it within 0.2 s. */
const PACE = 80;
/** The least shown per second while text waits, faster than models write, so the end of a burst never lingers. */
const MIN_RATE = 400;

/**
 * How much of `arrived` to show after `elapsed` milliseconds with `shown` characters shown: a burst spread over a few
 * tenths of a second, a steady stream about as it comes, always up to the end of a word.
 */
export function advance(shown: number, arrived: string, elapsed: number): number {
  const backlog = arrived.length - shown;
  if (backlog <= 0) {
    return arrived.length;
  }
  const step = Math.max(
    1,
    Math.ceil((backlog * Math.min(elapsed, PACE)) / PACE),
    Math.ceil((MIN_RATE * elapsed) / 1000),
  );
  let next = Math.min(arrived.length, shown + step);
  while (next < arrived.length && !/\s/.test(arrived[next])) {
    next++;
  }
  return next;
}

/**
 * The part of `text`, still being written, that can be shown as it will look: whole words, and nothing from an
 * unfinished construct on, such as `**bold`, `` `code ``, a link or an HTML tag. A code block still open shows as
 * code, line by line.
 */
export function settled(text: string): string {
  // Whole words: the last one may be cut.
  const lastSpace = Math.max(text.lastIndexOf(' '), text.lastIndexOf('\n'), text.lastIndexOf('\t'));
  let shown = text.slice(0, lastSpace + 1);

  // A fence line still being written, such as "```ba", is held back whole.
  const lineStart = shown.lastIndexOf('\n') + 1;
  const lastLine = shown.slice(lineStart);
  if (/^ {0,3}(`{1,}|~{1,})/.test(lastLine) && !shown.endsWith('\n')) {
    const fenceLike = lastLine.trimStart();
    if (/^(`{3,}|~{3,})/.test(fenceLike) || /^[`~]{1,2}$/.test(fenceLike.trim())) {
      shown = shown.slice(0, lineStart);
    }
  }
  const block = openBlock(shown);
  if (block !== null && block !== EXPLANATION) {
    return shown;
  }
  // Inline constructs end within their line: only the line being written can hold one still open.
  const from = shown.endsWith('\n') ? shown.length : shown.lastIndexOf('\n') + 1;
  const open = unclosed(shown.slice(from));
  return open < 0 ? shown : shown.slice(0, from + open);
}

/** The language of the fenced block `text` ends inside, `''` if it has none, or `null` outside any. */
function openBlock(text: string): string | null {
  let fence: string | null = null;
  let language = '';
  for (const line of text.split('\n')) {
    const found = line.match(/^ {0,3}(`{3,}|~{3,})(.*)$/);
    if (!found) {
      continue;
    }
    const [, marker, info] = found;
    if (fence === null) {
      fence = marker;
      language = info.trim().split(/\s/)[0] ?? '';
    } else if (marker[0] === fence[0] && marker.length >= fence.length && info.trim() === '') {
      fence = null;
    }
  }
  return fence === null ? null : language;
}

const WORD = /[\p{L}\p{N}]/u;
const SPACE = /\s/;

/** Where the first construct of `line` still open starts, or -1 if none is. */
function unclosed(line: string): number {
  // Emphasis openers waiting for their closer: `*`, `_` and `~` runs.
  const openers: { char: string; at: number }[] = [];
  let i = 0;
  while (i < line.length) {
    const char = line[i];
    if (char === '\\') {
      i += 2;
      continue;
    }
    if (char === '`') {
      const run = runLength(line, i, '`');
      const close = closingRun(line, i + run, run);
      if (close < 0) {
        return firstOpen(openers, i);
      }
      i = close + run;
      continue;
    }
    if (char === '<' && (i + 1 === line.length || /[A-Za-z/!?]/.test(line[i + 1]))) {
      const close = line.indexOf('>', i + 1);
      if (close < 0) {
        return firstOpen(openers, i);
      }
      i = close + 1;
      continue;
    }
    if (char === '[' || (char === '!' && line[i + 1] === '[')) {
      const start = i;
      const close = line.indexOf(']', i + 1);
      if (close < 0) {
        return firstOpen(openers, start);
      }
      i = close + 1;
      if (i === line.length) {
        // "[text]" may still become a link.
        return firstOpen(openers, start);
      }
      if (line[i] === '(') {
        const end = line.indexOf(')', i + 1);
        if (end < 0) {
          return firstOpen(openers, start);
        }
        i = end + 1;
      }
      continue;
    }
    if (char === '*' || char === '_' || char === '~') {
      const run = runLength(line, i, char);
      const before = line[i - 1];
      const after = line[i + run];
      if (after === undefined) {
        // A run at the end may open what comes next.
        return firstOpen(openers, i);
      }
      const intraword = char === '_' && before !== undefined && WORD.test(before) && WORD.test(after);
      const canClose = before !== undefined && !SPACE.test(before) && !intraword;
      const canOpen = !SPACE.test(after) && !intraword && !(char === '_' && before !== undefined && WORD.test(before));
      const match = canClose ? lastOpener(openers, char) : -1;
      if (match >= 0) {
        openers.length = match;
      } else if (canOpen) {
        openers.push({ char, at: i });
      }
      i += run;
      continue;
    }
    i++;
  }
  return openers.length > 0 ? openers[0].at : -1;
}

/** The last opener of `char` still waiting, or -1. */
function lastOpener(openers: { char: string }[], char: string): number {
  for (let index = openers.length - 1; index >= 0; index--) {
    if (openers[index].char === char) {
      return index;
    }
  }
  return -1;
}

/** The earlier of the first opener still waiting and `at`. */
function firstOpen(openers: { at: number }[], at: number): number {
  return openers.length > 0 ? Math.min(openers[0].at, at) : at;
}

/** How many `char` follow one another in `line` from `from`. */
function runLength(line: string, from: number, char: string): number {
  let end = from;
  while (line[end] === char) {
    end++;
  }
  return end - from;
}

/** Where a run of exactly `length` backticks starts in `line` from `from`, or -1. */
function closingRun(line: string, from: number, length: number): number {
  let i = from;
  while (i < line.length) {
    if (line[i] === '`') {
      const run = runLength(line, i, '`');
      if (run === length) {
        return i;
      }
      i += run;
    } else {
      i++;
    }
  }
  return -1;
}
