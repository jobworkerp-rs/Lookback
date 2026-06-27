// UI locale handling. Mirrors `lib/theme.ts`: the same shape of load/save/resolve
// so the cognitive load matches the theme toggle. NOTE: LOCALE_STORAGE_KEY and the
// resolve rule are mirrored by the lang-setting inline script in `index.html` — keep
// them in sync. This module deliberately does NOT import `@tauri-apps/api`: the
// preference lives only in localStorage, so "delete all data" (purge_all_data) never
// touches it (AC-I18N-6, verified structurally by locale.purge.test).

export type LocalePref = "system" | "ja" | "en";
export type ResolvedLocale = "ja" | "en";

/** Display labels for the locale toggle (Sidebar). Ordered system → ja → en. */
export const LOCALE_OPTIONS: { value: LocalePref; labelKey: string }[] = [
  { value: "system", labelKey: "locale.system" },
  { value: "ja", labelKey: "locale.ja" },
  { value: "en", labelKey: "locale.en" },
];

export const LOCALE_STORAGE_KEY = "lookback.locale";

const PREFS: readonly LocalePref[] = ["system", "ja", "en"];

function isLocalePref(v: unknown): v is LocalePref {
  return typeof v === "string" && (PREFS as readonly string[]).includes(v);
}

/**
 * The single source of Lookback's "is this Japanese?" rule: any tag starting
 * with "ja" (case-insensitive). Mirrored by the lang-setting inline script in
 * `index.html` — keep them in sync.
 */
export function isJaLang(lang: string): boolean {
  return lang.toLowerCase().startsWith("ja");
}

/** Read the saved preference; unset or malformed values fall back to "system". */
export function loadLocalePref(): LocalePref {
  try {
    const v = localStorage.getItem(LOCALE_STORAGE_KEY);
    return isLocalePref(v) ? v : "system";
  } catch {
    // Private mode / disabled storage — default rather than crash.
    return "system";
  }
}

export function saveLocalePref(pref: LocalePref): void {
  try {
    localStorage.setItem(LOCALE_STORAGE_KEY, pref);
  } catch {
    // Best-effort; a failed write just means the choice isn't persisted.
  }
}

/**
 * Resolve the effective locale. Pure (systemLang is passed in, not read from
 * navigator) so it is unit-testable. "system" follows the OS/browser locale:
 * anything starting with "ja" is Japanese, everything else is English.
 */
export function resolveLocale(pref: LocalePref, systemLang: string): ResolvedLocale {
  if (pref === "system") return isJaLang(systemLang) ? "ja" : "en";
  return pref;
}
