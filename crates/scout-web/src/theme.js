(() => {
  const storageKey = "scout-theme";
  const themes = new Set(["solarized-dark", "solarized-light", "light", "black"]);
  // System is the default and is never stored: no saved choice means "follow
  // the device", so a reader who picks System again is simply forgotten.
  // It resolves to the solarized pair, which is the page's own look.
  const system = "system";
  const lightQuery = "(prefers-color-scheme: light)";
  const root = document.documentElement;

  function savedChoice() {
    try {
      const saved = localStorage.getItem(storageKey);
      return themes.has(saved) ? saved : system;
    } catch {
      // Storage can be disabled; the device's setting still applies.
      return system;
    }
  }

  function systemTheme() {
    return window.matchMedia?.(lightQuery)?.matches ? "solarized-light" : "solarized-dark";
  }

  let choice = savedChoice();

  // Runs as the head is read, before the body is painted, so a light-mode
  // reader never sees a frame of the dark default first.
  function apply() {
    root.dataset.theme = choice === system ? systemTheme() : choice;
  }
  apply();

  // A device that switches to dark mode in the evening takes an open page
  // with it, while System is the choice.
  window.matchMedia?.(lightQuery)?.addEventListener?.("change", () => {
    if (choice === system) apply();
  });

  document.addEventListener("DOMContentLoaded", () => {
    const select = document.getElementById("theme-select");
    if (!select) return;

    select.value = choice;
    select.addEventListener("change", () => {
      const picked = select.value;
      if (picked !== system && !themes.has(picked)) return;

      choice = picked;
      apply();
      try {
        if (picked === system) localStorage.removeItem(storageKey);
        else localStorage.setItem(storageKey, picked);
      } catch {
        // Keep the current page's choice even if it cannot be persisted.
      }
    });
  });
})();
