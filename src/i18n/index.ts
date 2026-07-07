import i18next from "i18next";
import { initReactI18next } from "react-i18next";
import { loadLocalePref, resolveLocale } from "@/lib/locale";
import en from "./locales/en.json";
import ja from "./locales/ja.json";

// Dictionaries are bundled (synchronous load) and the language is resolved before
// first paint so there is no FOUC on switch. The initial language is
// the saved preference resolved against navigator.language; "en" is the fallback
// for any missing key. React already escapes interpolated values, so
// i18next's own escaping is disabled.
i18next.use(initReactI18next).init({
  resources: {
    ja: { translation: ja },
    en: { translation: en },
  },
  lng: resolveLocale(loadLocalePref(), navigator.language),
  fallbackLng: "en",
  interpolation: { escapeValue: false },
});

export default i18next;
