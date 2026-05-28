// App-wide theme: a single named palette ("Civic Archive") with a light/dark
// mode. Mode lives as the classic `dark` class on <html> (Tailwind's
// class-based variant) and persists to localStorage.

export type Theme = "light" | "dark";

const MODE_KEY = "palamedes-theme";

let _theme = $state<Theme>("dark");

function applyMode(t: Theme) {
  const root = document.documentElement;
  if (t === "dark") root.classList.add("dark");
  else root.classList.remove("dark");
}

export function initTheme() {
  let initialMode: Theme = "dark";
  try {
    const stored = localStorage.getItem(MODE_KEY) as Theme | null;
    if (stored === "light" || stored === "dark") {
      initialMode = stored;
    } else if (window.matchMedia?.("(prefers-color-scheme: light)").matches) {
      initialMode = "light";
    }
  } catch {
    // localStorage may be unavailable; fall back to defaults.
  }
  _theme = initialMode;
  applyMode(initialMode);
}

export const themeState = {
  get current(): Theme {
    return _theme;
  },
  set(t: Theme) {
    _theme = t;
    try {
      localStorage.setItem(MODE_KEY, t);
    } catch {
      // ignore
    }
    applyMode(t);
  },
  toggle() {
    this.set(_theme === "dark" ? "light" : "dark");
  },
};
