/** Saves `text` as the file `name`, through the browser's download. */
export function download(name: string, text: string, type: string): void {
  const url = URL.createObjectURL(new Blob([text], { type: `${type};charset=utf-8` }));
  const link = document.createElement('a');
  link.href = url;
  link.download = name;
  link.click();
  // Revoked once the browser has taken it.
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}
