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

  // Inside Telegram, Telegram's light or dark decides — not the device, and
  // not a choice made on the website, which this storage does not hold
  // anyway. Telegram hands its colours to the launch page in the address;
  // the launch page keeps the answer for the trip page it opens, which
  // says it is in Telegram with `data-surface`. Under the name
  // `telegram.js` exports as `TELEGRAM_SCHEME_KEY`.
  const telegramKey = "scout-telegram-scheme";

  // `telegram.js`'s `telegramTheme`, which this classic script cannot
  // import: Telegram's own line for a dark theme, a perceived brightness
  // under 120. theme.test.mjs holds the two copies to each other.
  function fromTelegramColour(bg) {
    const hex = /^#?([0-9a-f]{6})$/i.exec(String(bg ?? "").trim());
    if (!hex) return null;
    const n = parseInt(hex[1], 16);
    const [r, g, b] = [n >> 16, (n >> 8) & 255, n & 255];
    return Math.sqrt(0.299 * r * r + 0.587 * g * g + 0.114 * b * b) < 120 ? "solarized-dark" : "solarized-light";
  }

  function telegramTheme() {
    const params = new URLSearchParams(String(location.hash ?? "").slice(1)).get("tgWebAppThemeParams");
    if (params) {
      let theme = null;
      try {
        theme = fromTelegramColour(JSON.parse(params).bg_color);
      } catch {
        theme = null;
      }
      if (theme) {
        try {
          sessionStorage.setItem(telegramKey, theme);
        } catch {
          // This page is still right; the next one falls back to the device.
        }
        return theme;
      }
    }
    if (root.dataset.surface !== "telegram") return null;
    try {
      const kept = sessionStorage.getItem(telegramKey);
      return themes.has(kept) ? kept : null;
    } catch {
      return null;
    }
  }

  const inTelegram = telegramTheme();
  let choice = inTelegram ?? savedChoice();

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
