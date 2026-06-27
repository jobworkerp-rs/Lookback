import { useEffect, useState } from "react";
import {
  applyTheme,
  loadThemePref,
  PREFERS_DARK_QUERY,
  resolveTheme,
  saveThemePref,
  type ThemePref,
} from "@/lib/theme";

export interface ThemeControl {
  pref: ThemePref;
  setPref: (pref: ThemePref) => void;
}

/**
 * Owns theme application. Call once (in App). Re-applies on preference
 * change and, while on "system", follows the OS via the matchMedia
 * `change` event.
 */
export function useTheme(): ThemeControl {
  const [pref, setPrefState] = useState<ThemePref>(loadThemePref);

  useEffect(() => {
    const mql = window.matchMedia(PREFERS_DARK_QUERY);
    const sync = () => applyTheme(resolveTheme(pref, mql.matches));
    sync();
    mql.addEventListener("change", sync);
    return () => mql.removeEventListener("change", sync);
  }, [pref]);

  const setPref = (next: ThemePref) => {
    saveThemePref(next);
    setPrefState(next);
  };

  return { pref, setPref };
}
