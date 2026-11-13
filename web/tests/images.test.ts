import { describe, expect, test } from 'bun:test';
import { pastedName, splitImages } from '../src/lib/images';

describe('splitImages', () => {
  test('reads each image and the text read from it after what was written', () => {
    const content = [
      'Why does this fail?',
      '',
      '[Image: error.png, uploaded to `C:\\Kelpy\\workspace\\files\\error.png`]',
      'Text read from it by OCR:',
      '````text',
      'Access denied',
      'use ```sudo```',
      '````',
      '',
      '[Image: blank.png, uploaded to `/root/Kelpy/workspace/files/blank.png`]',
      'No text read from it by OCR.',
      '',
      '[Image: photo.jpg, uploaded to `/root/Kelpy/workspace/files/photo.jpg`]',
      'Its text could not be read: Tesseract is not installed.',
    ].join('\n');
    expect(splitImages(content)).toEqual({
      text: 'Why does this fail?',
      images: [
        {
          name: 'error.png',
          path: 'C:\\Kelpy\\workspace\\files\\error.png',
          text: 'Access denied\nuse ```sudo```',
          problem: null,
        },
        { name: 'blank.png', path: '/root/Kelpy/workspace/files/blank.png', text: '', problem: null },
        {
          name: 'photo.jpg',
          path: '/root/Kelpy/workspace/files/photo.jpg',
          text: null,
          problem: 'Tesseract is not installed',
        },
      ],
    });
  });

  test('an image alone, and a message that only mentions one, are told apart', () => {
    const alone = '[Image: shot.png, uploaded to `/f/shot.png`]\nNo text read from it by OCR.';
    expect(splitImages(alone).text).toBe('');
    expect(splitImages(alone).images).toHaveLength(1);
    const mentioned = 'Look:\n\n[Image: shot.png, uploaded to `/f/shot.png`]\nand then this';
    expect(splitImages(mentioned)).toEqual({ text: mentioned, images: [] });
    const unclosed = '[Image: shot.png, uploaded to `/f/shot.png`]\nText read from it by OCR:\n```text\nno end';
    expect(splitImages(unclosed).images).toHaveLength(0);
  });
});

describe('pastedName', () => {
  const now = new Date(2026, 10, 12, 15, 30, 12);
  const file = (name: string, type: string) => new File([new Uint8Array(1)], name, { type });

  test('a pasted screenshot is named after the moment, a named file keeps its name', () => {
    expect(pastedName(file('image.png', 'image/png'), now)).toBe('screenshot-2026-11-12-153012.png');
    expect(pastedName(file('image.jpeg', 'image/jpeg'), now)).toBe('screenshot-2026-11-12-153012.jpg');
    expect(pastedName(file('error dialog.png', 'image/png'), now)).toBe('error dialog.png');
  });
});
