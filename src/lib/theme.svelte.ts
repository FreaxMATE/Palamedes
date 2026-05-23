// App-wide theme: a light/dark mode + a named palette. Mode lives as the
// classic `dark` class on <html> (Tailwind's class-based variant); palette
// lives as a `data-palette` attribute so palette-specific CSS variables in
// `app.css` can scope themselves. Both persist to localStorage.

export type Theme = "light" | "dark";
export type Palette = "default" | "audit-dark" | "inkwell" | "atrium";

export const PALETTES: { id: Palette; label: string; blurb: string }[] = [
  { id: "default", label: "Default", blurb: "Tailwind neutrals + violet accent." },
  { id: "audit-dark", label: "Audit dark", blurb: "Near-black, violet accent — Linear-adjacent." },
  { id: "inkwell", label: "Inkwell", blurb: "Warm paper text on near-black — myth-on-screen." },
  { id: "atrium", label: "Atrium", blurb: "Graphite + mint — bold design opinion." },
];

const MODE_KEY = "palamedes-theme";
const PALETTE_KEY = "palamedes-palette";

let _theme = $state<Theme>("dark");
let _palette = $state<Palette>("default");

function applyMode(t: Theme) {
  const root = document.documentElement;
  if (t === "dark") root.classList.add("dark");
  else root.classList.remove("dark");
}

function applyPalette(p: Palette) {
  document.documentElement.setAttribute("data-palette", p);
}

export function initTheme() {
  let initialMode: Theme = "dark";
  let initialPalette: Palette = "default";
  try {
    const stored = localStorage.getItem(MODE_KEY) as Theme | null;
    if (stored === "light" || stored === "dark") {
      initialMode = stored;
    } else if (window.matchMedia?.("(prefers-color-scheme: light)").matches) {
      initialMode = "light";
    }
    const sp = localStorage.getItem(PALETTE_KEY) as Palette | null;
    if (sp && PALETTES.some((p) => p.id === sp)) initialPalette = sp;
  } catch {
    // localStorage may be unavailable; fall back to defaults.
  }
  _theme = initialMode;
  _palette = initialPalette;
  applyMode(initialMode);
  applyPalette(initialPalette);
}

export const themeState = {
  get current(): Theme {
    return _theme;
  },
  get palette(): Palette {
    return _palette;
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
  setPalette(p: Palette) {
    _palette = p;
    try {
      localStorage.setItem(PALETTE_KEY, p);
    } catch {
      // ignore
    }
    applyPalette(p);
  },
  toggle() {
    this.set(_theme === "dark" ? "light" : "dark");
  },
};
