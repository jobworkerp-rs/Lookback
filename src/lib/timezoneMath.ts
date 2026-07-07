/** Format `instant` in `tz` with the given `Intl.DateTimeFormat` options and
 *  return a `(type) -> numeric part` accessor. `undefined` for an invalid zone
 *  (the caller then falls back / signals NaN). Shared by every zone-aware
 *  helper below so the formatter construction + `formatToParts` lookup lives in
 *  one place. */
function zonedPartGetter(
  instant: Date,
  tz: string,
  options: Intl.DateTimeFormatOptions,
): ((type: string) => number) | undefined {
  let dtf: Intl.DateTimeFormat;
  try {
    dtf = new Intl.DateTimeFormat("en-US", { timeZone: tz, ...options });
  } catch {
    // Invalid IANA name.
    return undefined;
  }
  const parts = dtf.formatToParts(instant);
  return (type: string) => Number(parts.find((p) => p.type === type)?.value);
}

/** The offset (ms) to ADD to a UTC instant to get the wall-clock time in `tz`
 *  at that instant. Derived by formatting the instant in `tz` and diffing the
 *  rendered wall-clock against the instant read as UTC. `NaN` for an invalid
 *  zone (the caller then falls back to the host zone). */
export function zoneOffsetMs(utcMs: number, tz: string): number {
  const get = zonedPartGetter(new Date(utcMs), tz, {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
  // Invalid IANA name: signal via NaN so callers can keep the host-zone path.
  if (!get) return Number.NaN;
  let hour = get("hour");
  // Intl may render midnight as "24" in some engines; normalise to 0.
  if (hour === 24) hour = 0;
  const asUtc = Date.UTC(
    get("year"),
    get("month") - 1,
    get("day"),
    hour,
    get("minute"),
    get("second"),
  );
  return asUtc - utcMs;
}

/** Calendar date parts for `instant` as rendered in an IANA time zone. */
export function datePartsInTimeZone(
  instant: Date,
  timeZone?: string,
): { year: number; month: number; day: number } | undefined {
  const tz = timeZone?.trim();
  if (!tz) return undefined;
  const get = zonedPartGetter(instant, tz, {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
  });
  if (!get) return undefined;
  const year = get("year");
  const month = get("month");
  const day = get("day");
  if (!year || !month || !day) return undefined;
  return { year, month, day };
}

/** Epoch ms of the first instant in `YYYY-MM-DD` in `tz`.
 *
 *  Resolve from the UTC-as-if wall time, then re-evaluate the offset at the
 *  first candidate. Some real zones move clocks at local 00:00, so using the
 *  UTC-midnight offset once can point at the previous calendar date.
 */
export function zonedMidnightMs(y: number, m: number, d: number, tz: string): number {
  const asUtcMidnight = Date.UTC(y, m - 1, d, 0, 0, 0);
  const off = zoneOffsetMs(asUtcMidnight, tz);
  if (Number.isNaN(off)) return Number.NaN;
  const candidate = asUtcMidnight - off;
  const candidateParts = datePartsInTimeZone(new Date(candidate), tz);
  if (candidateParts?.year === y && candidateParts.month === m && candidateParts.day === d) {
    return candidate;
  }
  const candidateOff = zoneOffsetMs(candidate, tz);
  if (Number.isNaN(candidateOff)) return Number.NaN;
  return asUtcMidnight - candidateOff;
}
