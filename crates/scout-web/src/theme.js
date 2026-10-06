(() => {
  const storageKey = "scout-theme";
  const defaultTheme = "solarized-dark";
  const themes = new Set([defaultTheme, "solarized-light", "light", "black"]);

  try {
    const savedTheme = localStorage.getItem(storageKey);
    if (themes.has(savedTheme)) {
      document.documentElement.dataset.theme = savedTheme;
    }
  } catch {
    // Storage can be disabled; the landing page still works with its default.
  }

  document.addEventListener("DOMContentLoaded", () => {
    const select = document.getElementById("theme-select");
    if (!select) return;

    select.value = document.documentElement.dataset.theme || defaultTheme;
    select.addEventListener("change", () => {
      const theme = select.value;
      if (!themes.has(theme)) return;

      document.documentElement.dataset.theme = theme;
      try {
        localStorage.setItem(storageKey, theme);
      } catch {
        // Keep the current page's choice even if it cannot be persisted.
      }
    });
  });
})();
