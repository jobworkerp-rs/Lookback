import { useTranslation } from "react-i18next";

/**
 * The active i18next locale tag, for passing to `formatDateTime`/`formatNumber`.
 * Hides the `resolvedLanguage ?? language` fallback (resolvedLanguage is
 * undefined before init) so components never reach into i18next internals.
 */
export function useLocaleTag(): string {
  const { i18n } = useTranslation();
  return i18n.resolvedLanguage ?? i18n.language;
}
