import { describe, expect, test } from 'bun:test';
import { marked } from 'marked';
import { advance, settled } from '../src/lib/reveal';

/** An answer using every construct that could show half-written. */
const answer = `Here is **how** to check the \`nginx\` service, with _care_ and ~~old~~ steps.

1. Run \`systemctl status nginx\` to see **whether it runs**, or ***both***.
2. Read [the docs](https://nginx.org/en/docs/) and press <kbd>Ctrl</kbd> with \`\`a \` b\`\`.
3. See ![the logo](https://nginx.org/logo.png) too.

\`\`\`bash
sudo systemctl restart nginx
cat /etc/nginx/sites_enabled/my_site *.conf
\`\`\`

Then check the folder *sites-enabled* and the file \`my_site.conf\`.`;

/** The text a reader sees once `markdown` is rendered: code left out, as it may hold any sign. */
function visibleText(markdown: string): string {
  const html = marked.parse(markdown, { async: false, gfm: true });
  return html
    .replace(/<pre>[\s\S]*?<\/pre>/g, '')
    .replace(/<code>[\s\S]*?<\/code>/g, '')
    .replace(/<[^>]+>/g, '')
    .replace(/&lt;/g, '<')
    .replace(/&gt;/g, '>')
    .replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'")
    .replace(/&amp;/g, '&');
}

describe('settled', () => {
  test('no prefix shows a sign of Markdown or HTML, and the text shown only grows', () => {
    let before = '';
    for (let length = 0; length <= answer.length; length++) {
      const prefix = answer.slice(0, length);
      const shown = settled(prefix);
      expect(prefix.startsWith(shown)).toBe(true);
      expect(shown.startsWith(before)).toBe(true);
      const seen = visibleText(shown);
      expect({ prefix, seen }).toEqual({ prefix, seen: seen.replace(/[*_`~[\]<>]/g, '') });
      before = shown;
    }
    // Once all has arrived, all of it is shown but the last word, which may go on.
    expect(settled(answer)).toBe(answer.slice(0, answer.lastIndexOf(' ') + 1));
  });

  test('an open code block shows as code, line by line', () => {
    expect(settled('Run:\n\n```bash\nsudo apt update\nsudo ap')).toBe('Run:\n\n```bash\nsudo apt update\nsudo ');
    expect(settled('Run:\n\n```ba')).toBe('Run:\n\n');
  });

  test('inside an explanation panel, Markdown still open is held back', () => {
    const panel = 'Restart it.\n\n```explanation\n';
    expect(settled(`${panel}This **restarts the `)).toBe(`${panel}This `);
    expect(settled(`${panel}This **restarts** the `)).toBe(`${panel}This **restarts** the `);
  });

  test('signs that open nothing are shown', () => {
    expect(settled('2 * 3 is six, and 4 < 5 too. ')).toBe('2 * 3 is six, and 4 < 5 too. ');
    expect(settled('Use my_site and snake_case names. ')).toBe('Use my_site and snake_case names. ');
    expect(settled('- one\n- two\n* three ')).toBe('- one\n- two\n* three ');
  });

  test('a construct is held back from where it opens', () => {
    expect(settled('Check **the status of ')).toBe('Check ');
    expect(settled('Read [the docs](https://ex ')).toBe('Read ');
    expect(settled('Press <kbd class ')).toBe('Press ');
    expect(settled('Run `systemctl status ')).toBe('Run ');
  });
});

describe('advance', () => {
  const text = 'one two three four five six seven eight nine ten '.repeat(10);

  test('a burst is spread over a few tenths of a second, word by word', () => {
    let shown = 0;
    const steps: number[] = [];
    for (let frame = 0; frame < 25 && shown < text.length; frame++) {
      shown = advance(shown, text, 16);
      steps.push(shown);
      expect(shown === text.length || /\s/.test(text[shown])).toBe(true);
    }
    expect(steps.length).toBeGreaterThan(5);
    expect(shown).toBe(text.length);
  });

  test('a short piece shows at once', () => {
    expect(advance(0, 'Hi there', 16)).toBe(8);
    expect(advance(8, 'Hi there', 16)).toBe(8);
  });
});
