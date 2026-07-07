import { useEffect, useState } from "react";
import i18n from "@/i18n";
import { type LocalePref, loadLocalePref, resolveLocale, saveLocalePref } from "@/lib/locale";

export interface LocaleControl {
  pref: LocalePref;
  resolved: "ja" | "en";
  setPref: (pref: LocalePref) => void;
}

/**
 * Owns locale application. Call once (in App), mirroring useTheme. Re-applies on
 * preference change by switching the i18next language and reflecting the effective
 * locale onto <html lang>. While on "system" it follows the OS via the
 * `languagechange` event (the locale analogue of useTheme's matchMedia listener).
 */
export function useLocale(): LocaleControl {
  const [pref, setPrefState] = useState<LocalePref>(loadLocalePref);
  const [resolved, setResolved] = useState<"ja" | "en">(() =>
    resolveLocale(loadLocalePref(), navigator.language),
  );

  useEffect(() => {
    const sync = () => {
      const next = resolveLocale(pref, navigator.language);
      setResolved(next);
      i18n.changeLanguage(next);
      document.documentElement.lang = next;
    };
    sync();
    // Only "system" needs to track OS language changes; explicit picks are fixed.
    if (pref !== "system") return;
    window.addEventListener("languagechange", sync);
    return () => window.removeEventListener("languagechange", sync);
  }, [pref]);

  const setPref = (next: LocalePref) => {
    saveLocalePref(next);
    setPrefState(next);
  };

  return { pref, resolved, setPref };
}
