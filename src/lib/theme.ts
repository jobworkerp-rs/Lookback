// Theme handling. The dark palette already exists in `styles/tokens.css`
// under `[data-theme="dark"]`; this module is the missing mechanism that
// toggles that attribute on <html> based on the user's preference and the
// OS setting. NOTE: STORAGE_KEY and the resolve rule are mirrored by the
// flash-prevention inline script in `index.html` — keep them in sync.

export type ThemePref = "system" | "light" | "dark";
export type ResolvedTheme = "light" | "dark";

/** Translation keys for the theme toggle (Sidebar). Ordered system → light → dark. */
export const THEME_OPTIONS: { value: ThemePref; labelKey: string }[] = [
  { value: "system", labelKey: "theme.option.system" },
  { value: "light", labelKey: "theme.option.light" },
  { value: "dark", labelKey: "theme.option.dark" },
];

export const THEME_STORAGE_KEY = "lookback.theme";
export const PREFERS_DARK_QUERY = "(prefers-color-scheme: dark)";

const PREFS: readonly ThemePref[] = ["system", "light", "dark"];

function isThemePref(v: unknown): v is ThemePref {
  return typeof v === "string" && (PREFS as readonly string[]).includes(v);
}

/** Read the saved preference; unset or malformed values fall back to "system". */
export function loadThemePref(): ThemePref {
  try {
    const v = localStorage.getItem(THEME_STORAGE_KEY);
    return isThemePref(v) ? v : "system";
  } catch {
    // Private mode / disabled storage — default rather than crash.
    return "system";
  }
}

export function saveThemePref(pref: ThemePref): void {
  try {
    localStorage.setItem(THEME_STORAGE_KEY, pref);
  } catch {
    // Best-effort; a failed write just means the choice isn't persisted.
  }
}

/** Resolve the effective theme. Pure (no matchMedia) so it is unit-testable. */
export function resolveTheme(pref: ThemePref, systemPrefersDark: boolean): ResolvedTheme {
  if (pref === "system") return systemPrefersDark ? "dark" : "light";
  return pref;
}

/** Reflect the resolved theme onto <html> (data-theme + native color-scheme). */
export function applyTheme(resolved: ResolvedTheme): void {
  const root = document.documentElement;
  if (resolved === "dark") root.setAttribute("data-theme", "dark");
  else root.removeAttribute("data-theme");
  root.style.colorScheme = resolved;
}
