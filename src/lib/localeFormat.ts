import { isJaLang, type ResolvedLocale } from "./locale";

export type IntlLocaleInput = string | null | undefined;

/** Narrow an arbitrary Intl tag to one of Lookback's two supported locales. */
function resolveIntlLocale(locale: IntlLocaleInput): ResolvedLocale {
  return locale && isJaLang(locale) ? "ja" : "en";
}

export function formatDateTime(ms: number, locale: IntlLocaleInput): string {
  return new Date(ms).toLocaleString(resolveIntlLocale(locale));
}

export function formatNumber(value: number, locale: IntlLocaleInput): string {
  return value.toLocaleString(resolveIntlLocale(locale));
}
