/** Saves `text` as the file `name`, through the browser's download. */
export function download(name: string, text: string, type: string): void {
  downloadBlob(name, new Blob([text], { type: `${type};charset=utf-8` }));
}

/** Saves `blob` as the file `name`, through the browser's download. */
export function downloadBlob(name: string, blob: Blob): void {
  const url = URL.createObjectURL(blob);
  const link = document.createElement('a');
  link.href = url;
  link.download = name;
  link.click();
  // Revoked once the browser has taken it.
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}
