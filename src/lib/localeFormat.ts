import { isJaLang, type ResolvedLocale } from "./locale";

export type IntlLocaleInput = string | null | undefined;

/** IANA timezone name (e.g. `"Asia/Tokyo"`) or `null`/`undefined` to follow the
 *  host/OS zone — the input to `formatDateTime`'s `timeZone` option. */
export type TimezoneInput = string | null | undefined;

/** Narrow an arbitrary Intl tag to one of Lookback's two supported locales. */
function resolveIntlLocale(locale: IntlLocaleInput): ResolvedLocale {
  return locale && isJaLang(locale) ? "ja" : "en";
}

/** Format an epoch-ms instant for display. When `timeZone` is a non-empty IANA
 *  name the instant is rendered in THAT zone (so the app-wide timezone setting
 *  is honoured, not just the browser/OS zone); otherwise it falls back to the
 *  host zone. A bad IANA name would make `toLocaleString` throw `RangeError`,
 *  so an invalid `timeZone` is silently ignored (host-zone render) rather than
 *  crashing the whole list. */
export function formatDateTime(
  ms: number,
  locale: IntlLocaleInput,
  timeZone?: TimezoneInput,
): string {
  const resolvedLocale = resolveIntlLocale(locale);
  const tz = timeZone?.trim();
  if (tz) {
    try {
      return new Date(ms).toLocaleString(resolvedLocale, { timeZone: tz });
    } catch {
      // Invalid IANA name — fall through to the host-zone render.
    }
  }
  return new Date(ms).toLocaleString(resolvedLocale);
}

export function formatNumber(value: number, locale: IntlLocaleInput): string {
  return value.toLocaleString(resolveIntlLocale(locale));
}
