// `<input type="date">` yields a local-TZ calendar date ("YYYY-MM-DD").
// Sending it as UTC midnight (e.g. `Date.parse("YYYY-MM-DD")` or appending
// `T00:00:00Z`) shifts the filter boundary by the local UTC offset — in JST
// (UTC+9) the user's "May 1" leaks the first 9 hours of that day. These
// helpers anchor the date to local midnight before crossing the IPC boundary.

/** "YYYY-MM-DD" (local date) -> epoch ms at local-TZ midnight. */
export function localDateToEpochMs(yyyymmdd: string): number | undefined {
  const [y, m, d] = yyyymmdd.split("-").map(Number);
  // Fast-reject obviously out-of-range parts before constructing a Date.
  if (!y || !m || m > 12 || !d || d > 31) return undefined;
  // `new Date(y, m-1, d)` interprets the components in the local TZ,
  // unlike the UTC-based `Date.parse("YYYY-MM-DD")`.
  const date = new Date(y, m - 1, d);
  // Authoritative check: the constructor silently rolls over invalid
  // dates (month 13 -> next Jan, Feb 31 -> Mar) and yields NaN getters
  // for malformed input — both anchor the filter to the wrong day, so
  // reject anything that didn't round-trip to the requested month/day.
  if (date.getMonth() !== m - 1 || date.getDate() !== d) return undefined;
  return date.getTime();
}

/** "YYYY-MM-DD" (local date) -> ISO 8601 of local-TZ midnight (UTC representation). */
export function localDateToIsoUtc(yyyymmdd: string): string | undefined {
  const ms = localDateToEpochMs(yyyymmdd);
  return ms === undefined ? undefined : new Date(ms).toISOString();
}

/** Today minus N days, formatted "YYYY-MM-DD" in local TZ (for <input type="date"> default). */
export function localTodayMinusDays(days: number): string {
  const d = new Date();
  d.setDate(d.getDate() - days);
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

/** Per-thread epoch-ms window for a `[from, to]` calendar-date range.
 *
 *  The memories thread filter uses strict `>` for `updated_after` and
 *  inclusive `<=` for `updated_before`, so both ends are nudged by 1ms to
 *  make the range "inclusive of both calendar days":
 *    - after  = from's local 00:00 − 1   (keeps the `from` day's 00:00:00.000)
 *    - before = (to + 1 day)'s local 00:00 − 1 (keeps the `to` day's last ms)
 *
 *  The `to` end advances by a calendar day via `setDate` (not a fixed +24h):
 *  on a DST transition the next local midnight is 23h/25h away, so adding a
 *  constant day would over- or under-shoot the boundary. Both ends are
 *  optional and independent; an absent end yields `undefined` (unbounded). */
export function dayRangeToEpochMs(
  from?: string,
  to?: string,
): { after: number | undefined; before: number | undefined } {
  const fromMs = from ? localDateToEpochMs(from) : undefined;
  const toMs = to ? localDateToEpochMs(to) : undefined;
  let before: number | undefined;
  if (toMs !== undefined) {
    const next = new Date(toMs);
    next.setDate(next.getDate() + 1);
    before = next.getTime() - 1;
  }
  return {
    after: fromMs === undefined ? undefined : fromMs - 1,
    before,
  };
}

/** Day-boundary tz offset (hours) for the period *single* workflows.
 *
 *  The OS offset is preferred when it is a whole number of hours
 *  (`getTimezoneOffset` returns minutes WEST of UTC, so negate and divide).
 *  Fractional zones (e.g. India +5:30) or an explicit `override` outside the
 *  integer 0–23 range the memories workflow accepts fall back to `fallback`
 *  (default 9 = JST). This only sets each summary's day boundary; it does NOT
 *  affect range selection, which is computed from date strings (TZ-independent). */
export function resolveTimezoneOffsetHours(override?: number, fallback = 9): number {
  if (override !== undefined && Number.isInteger(override) && override >= 0 && override <= 23) {
    return override;
  }
  const sys = -new Date().getTimezoneOffset() / 60;
  if (Number.isInteger(sys) && sys >= 0 && sys <= 23) return sys;
  return fallback;
}
