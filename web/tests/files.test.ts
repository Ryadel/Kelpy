import { describe, expect, test } from 'bun:test';
import { size } from '../src/lib/files';

describe('size', () => {
  test('bytes stay whole, larger sizes take one decimal in binary units', () => {
    expect(size(840)).toBe('840 B');
    expect(size(12_700)).toBe('12.4 KiB');
    expect(size(3.1 * 1024 * 1024)).toBe('3.1 MiB');
    expect(size(2 * 1024 ** 3)).toBe('2.0 GiB');
  });
});
