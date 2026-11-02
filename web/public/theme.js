// Applies the saved theme before the page is drawn, so it never flashes: dark unless the user chose light.
// The key is shared with ThemeToggle.svelte.
if (localStorage.getItem('kelpy.theme') === 'light') {
  document.documentElement.classList.remove('dark');
}
