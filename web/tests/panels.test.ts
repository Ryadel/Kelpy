import { describe, expect, test } from 'bun:test';
import { COMMAND_LOG, SIDEBAR, panelWidth, startWidth, storedWidth } from '../src/lib/panels';

describe('panel widths', () => {
  test('are held within their limits and rounded', () => {
    expect(panelWidth(SIDEBAR, 10)).toBe(SIDEBAR.min);
    expect(panelWidth(SIDEBAR, 2000)).toBe(SIDEBAR.max);
    expect(panelWidth(COMMAND_LOG, 2000)).toBe(COMMAND_LOG.max);
    expect(panelWidth(SIDEBAR, 300.6)).toBe(301);
  });

  test('fall back to the initial width when the browser keeps none or nonsense', () => {
    expect(storedWidth(SIDEBAR, null)).toBe(SIDEBAR.initial);
    expect(storedWidth(COMMAND_LOG, 'wide')).toBe(COMMAND_LOG.initial);
    expect(storedWidth(SIDEBAR, '320')).toBe(320);
    expect(storedWidth(SIDEBAR, '9999')).toBe(SIDEBAR.max);
  });
});

test('a drag starts from the width shown when the window holds the panel narrower', () => {
  // Kept at 480, shown at 328 in a narrow window: the drag starts at 328, so the edge moves at once.
  expect(startWidth(480, 328.4)).toBe(328);
  expect(startWidth(300, 300)).toBe(300);
  expect(startWidth(300, undefined)).toBe(300);
  expect(startWidth(300, 0)).toBe(300);
  expect(startWidth(300, Number.NaN)).toBe(300);
});
