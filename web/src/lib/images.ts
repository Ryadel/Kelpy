/** An image shown with a message, as its stored text says. */
export interface MessageImage {
  /** Its name among the uploaded files. */
  name: string;
  /** Where it is on the server. */
  path: string;
  /** The text read from it by OCR: empty when none was found, null when it could not be read. */
  text: string | null;
  /** Why its text could not be read. */
  problem: string | null;
}

const IMAGE_LINE = /^\[Image: (.+?), uploaded to `(.+)`\]$/;
const READ = 'Text read from it by OCR:';
const NONE_READ = 'No text read from it by OCR.';
const NOT_READ = 'Its text could not be read: ';

/**
 * A user's message as stored, split into what the administrator wrote and the images shown with it: Kelpy stores
 * each image as a line naming it, followed by the text read from it. A message whose end does not read so has no
 * images.
 */
export function splitImages(content: string): { text: string; images: MessageImage[] } {
  const lines = content.split('\n');
  for (let start = 0; start < lines.length; start += 1) {
    if (!IMAGE_LINE.test(lines[start]) || (start > 0 && lines[start - 1] !== '')) {
      continue;
    }
    const images = imageBlocks(lines, start);
    if (images) {
      return { text: lines.slice(0, start).join('\n').trimEnd(), images };
    }
  }
  return { text: content, images: [] };
}

/** The images from line `from` to the end, or null if the lines are not all image blocks. */
function imageBlocks(lines: string[], from: number): MessageImage[] | null {
  const images: MessageImage[] = [];
  let at = from;
  while (at < lines.length) {
    if (lines[at] === '') {
      at += 1;
      continue;
    }
    const named = IMAGE_LINE.exec(lines[at]);
    if (!named) {
      return null;
    }
    const image: MessageImage = { name: named[1], path: named[2], text: null, problem: null };
    const next = lines[at + 1] ?? '';
    if (next === READ) {
      const fence = /^(`{3,})text$/.exec(lines[at + 2] ?? '');
      const end = fence ? lines.indexOf(fence[1], at + 3) : -1;
      if (!fence || end < 0) {
        return null;
      }
      image.text = lines.slice(at + 3, end).join('\n');
      at = end + 1;
    } else if (next === NONE_READ) {
      image.text = '';
      at += 2;
    } else if (next.startsWith(NOT_READ)) {
      image.problem = next.slice(NOT_READ.length).replace(/\.$/, '');
      at += 2;
    } else {
      at += 1;
    }
    images.push(image);
  }
  return images.length > 0 ? images : null;
}

/** Whether `file` is an image Kelpy shows the model. */
export const isImage = (file: File) => ['image/png', 'image/jpeg', 'image/gif', 'image/webp'].includes(file.type);

/**
 * The name to upload a pasted image under: the clipboard calls every screenshot `image.png`, so it is named after
 * the moment it was pasted, as `screenshot-2026-11-12-153012.png`.
 */
export function pastedName(file: File, now: Date): string {
  if (!/^image\.\w+$/i.test(file.name) && file.name !== '') {
    return file.name;
  }
  const two = (value: number) => String(value).padStart(2, '0');
  const day = `${now.getFullYear()}-${two(now.getMonth() + 1)}-${two(now.getDate())}`;
  const time = `${two(now.getHours())}${two(now.getMinutes())}${two(now.getSeconds())}`;
  const extension = file.type === 'image/jpeg' ? 'jpg' : (file.type.split('/')[1] ?? 'png');
  return `screenshot-${day}-${time}.${extension}`;
}
