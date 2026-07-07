// `<input type="date">` yields a calendar date ("YYYY-MM-DD"). Sending it as
// UTC midnight (e.g. `Date.parse("YYYY-MM-DD")` or appending `T00:00:00Z`)
// shifts the filter boundary by the zone's UTC offset — in JST (UTC+9) the
// user's "May 1" leaks the first 9 hours of that day. These helpers anchor the
// date to midnight in the DISPLAY zone before crossing the IPC boundary.
//
// The optional `timeZone` (an IANA name) is the app-wide display timezone. When
// given, the boundary is computed in THAT zone so it matches the timestamps the
// UI renders (which pass the same zone to `formatDateTime`); when omitted it
// falls back to the host/OS zone (the historical behaviour). A blank/invalid
// zone also falls back to the host zone rather than throwing.

import { zonedMidnightMs, zoneOffsetMs } from "./timezoneMath";

/** A Date's UTC y/m/d as "YYYY-MM-DD". Callers pre-shift the Date so its UTC
 *  fields carry the wall-clock they want rendered (a display zone or a UTC-
 *  anchored calendar day), keeping the shared padStart formatting in one spot. */
function formatUtcYmd(date: Date): string {
  const y = date.getUTCFullYear();
  const m = String(date.getUTCMonth() + 1).padStart(2, "0");
  const d = String(date.getUTCDate()).padStart(2, "0");
  return `${y}-${m}-${d}`;
}

/** A Date's host-local y/m/d as "YYYY-MM-DD" (the host-zone counterpart of
 *  `formatUtcYmd`), sharing the padStart formatting without a UTC round-trip. */
function formatLocalYmd(date: Date): string {
  const y = date.getFullYear();
  const m = String(date.getMonth() + 1).padStart(2, "0");
  const d = String(date.getDate()).padStart(2, "0");
  return `${y}-${m}-${d}`;
}

/** "YYYY-MM-DD" -> epoch ms at midnight in `timeZone` (or the host zone when
 *  `timeZone` is omitted / blank / invalid). */
export function localDateToEpochMs(yyyymmdd: string, timeZone?: string): number | undefined {
  const [y, m, d] = yyyymmdd.split("-").map(Number);
  // Fast-reject obviously out-of-range parts before constructing a Date.
  if (!y || !m || m > 12 || !d || d > 31) return undefined;

  const tz = timeZone?.trim();
  if (tz) {
    const ms = zonedMidnightMs(y, m, d, tz);
    if (!Number.isNaN(ms)) {
      // Round-trip validity check in `tz`: reject silent rollovers (month 13,
      // Feb 31) by confirming the epoch renders back to the requested y/m/d.
      const off = zoneOffsetMs(ms, tz);
      const wall = new Date(ms + off);
      if (wall.getUTCFullYear() === y && wall.getUTCMonth() === m - 1 && wall.getUTCDate() === d) {
        return ms;
      }
      return undefined;
    }
    // Invalid zone: fall through to the host-zone path.
  }

  // `new Date(y, m-1, d)` interprets the components in the host TZ,
  // unlike the UTC-based `Date.parse("YYYY-MM-DD")`.
  const date = new Date(y, m - 1, d);
  // Authoritative check: the constructor silently rolls over invalid
  // dates (month 13 -> next Jan, Feb 31 -> Mar) and yields NaN getters
  // for malformed input — both anchor the filter to the wrong day, so
  // reject anything that didn't round-trip to the requested month/day.
  if (date.getMonth() !== m - 1 || date.getDate() !== d) return undefined;
  return date.getTime();
}

/** "YYYY-MM-DD" -> ISO 8601 of `timeZone` (or host-zone) midnight, UTC form. */
export function localDateToIsoUtc(yyyymmdd: string, timeZone?: string): string | undefined {
  const ms = localDateToEpochMs(yyyymmdd, timeZone);
  return ms === undefined ? undefined : new Date(ms).toISOString();
}

/** Today minus N days, formatted "YYYY-MM-DD" in `timeZone` (or the host zone
 *  when omitted). For an `<input type="date">` default. */
export function localTodayMinusDays(days: number, timeZone?: string): string {
  const tz = timeZone?.trim();
  if (tz) {
    // Format "now" in `tz` to get today's wall-clock date there, then step back
    // on the calendar (via a UTC-anchored Date so `setUTCDate` does the day
    // arithmetic without re-crossing a zone boundary).
    const off = zoneOffsetMs(Date.now(), tz);
    if (!Number.isNaN(off)) {
      const todayThere = new Date(Date.now() + off);
      todayThere.setUTCDate(todayThere.getUTCDate() - days);
      return formatUtcYmd(todayThere);
    }
  }
  // Host-zone path: step back on the host calendar and render its wall-clock
  // date directly.
  const d = new Date();
  d.setDate(d.getDate() - days);
  return formatLocalYmd(d);
}

/** Per-thread epoch-ms window for a `[from, to]` calendar-date range.
 *
 *  The memories thread filter uses strict `>` for `updated_after` and
 *  inclusive `<=` for `updated_before`, so both ends are nudged by 1ms to
 *  make the range "inclusive of both calendar days":
 *    - after  = from's local 00:00 − 1   (keeps the `from` day's 00:00:00.000)
 *    - before = (to + 1 day)'s local 00:00 − 1 (keeps the `to` day's last ms)
 *
 *  The `to` end advances by a calendar day on the STRING date (not a fixed
 *  +24h): on a DST transition the next midnight is 23h/25h away, so adding a
 *  constant day would over- or under-shoot the boundary. Stepping the date
 *  string and re-resolving its midnight in `timeZone` keeps the boundary exact
 *  across DST. Both ends are optional and independent; an absent end yields
 *  `undefined` (unbounded). */
export function dayRangeToEpochMs(
  from?: string,
  to?: string,
  timeZone?: string,
): { after: number | undefined; before: number | undefined } {
  const fromMs = from ? localDateToEpochMs(from, timeZone) : undefined;
  let before: number | undefined;
  if (to) {
    const toMs = localDateToEpochMs(to, timeZone);
    if (toMs !== undefined) {
      const nextDayMs = localDateToEpochMs(nextCalendarDate(to), timeZone);
      if (nextDayMs !== undefined) before = nextDayMs - 1;
    }
  }
  return {
    after: fromMs === undefined ? undefined : fromMs - 1,
    before,
  };
}

/** "YYYY-MM-DD" -> the next calendar day as "YYYY-MM-DD". Pure string/UTC date
 *  arithmetic (zone-independent: a calendar day's successor is the same in
 *  every zone), so it can be re-resolved to midnight in any target zone. */
function nextCalendarDate(yyyymmdd: string): string {
  const [y, m, d] = yyyymmdd.split("-").map(Number);
  if (y === undefined || m === undefined || d === undefined) return yyyymmdd;
  const next = new Date(Date.UTC(y, m - 1, d));
  next.setUTCDate(next.getUTCDate() + 1);
  return formatUtcYmd(next);
}

/** Day-boundary tz offset (hours) for the period *single* workflows.
 *
 *  When `timeZone` (an IANA name) is given, its whole-hour offset from UTC is
 *  used so the fallback "yesterday / last week / last month" is computed in the
 *  DISPLAY zone (matching the workflow's own `env.TZ` boundary). Otherwise the
 *  OS offset is used (`getTimezoneOffset` returns minutes WEST of UTC, so negate
 *  and divide). Fractional zones (e.g. India +5:30), a negative/out-of-range
 *  offset, or an explicit `override` outside 0–23 fall back to `fallback`
 *  (default 9 = JST) — the memories workflow's `timezone_offset_hours` accepts
 *  only 0–23, and this fallback path only fires when `env.TZ` is unset. */
export function resolveTimezoneOffsetHours(
  override?: number,
  fallback = 9,
  timeZone?: string,
): number {
  if (override !== undefined && Number.isInteger(override) && override >= 0 && override <= 23) {
    return override;
  }
  const tz = timeZone?.trim();
  if (tz) {
    const off = zoneOffsetMs(Date.now(), tz);
    if (!Number.isNaN(off)) {
      // `zoneOffsetMs` compares a second-precision wall clock against a
      // sub-second `now`, so the raw ms carries a fractional-second remainder.
      // Zone offsets are always whole minutes, so round to minutes first, then
      // check for a whole hour (rejects +5:30 etc.).
      const offMinutes = Math.round(off / 60_000);
      const hours = offMinutes / 60;
      if (Number.isInteger(hours) && hours >= 0 && hours <= 23) return hours;
    }
  }
  // `+ 0` normalizes a `-0` (when the OS offset is 0) to `+0`, which callers
  // and tests compare with `Object.is`-based equality.
  const sys = -new Date().getTimezoneOffset() / 60 + 0;
  if (Number.isInteger(sys) && sys >= 0 && sys <= 23) return sys;
  return fallback;
}
