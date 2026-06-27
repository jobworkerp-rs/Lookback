import type { SummaryKind } from "@/types/api";

// Period summaries are keyed by an `external_id` period token:
//   - daily:   "YYYY-MM-DD"
//   - weekly:  "YYYY-Www"   (ISO 8601 week, Monday-based)
//   - monthly: "YYYY-MM"
// These helpers parse those tokens and lay out a month grid for the calendar.
// All dates are local-TZ anchored, matching `dateInput.ts` (the daily token
// is a calendar date, so UTC parsing would shift it by the local offset).

/** "YYYY-MM" -> {y,m}, or null when malformed. */
export function monthKeyToYearMonth(key: string): { y: number; m: number } | null {
  const match = /^(\d{4})-(\d{2})$/.exec(key);
  if (!match) return null;
  const y = Number(match[1]);
  const m = Number(match[2]);
  if (m < 1 || m > 12) return null;
  return { y, m };
}

/**
 * "YYYY-Www" (ISO 8601 week) -> the Date of that week's Monday (local TZ),
 * or null when malformed. ISO weeks are Monday-based and week 1 is the week
 * containing January 4th, so W01 can start in the previous calendar year.
 */
export function isoWeekKeyToMondayDate(key: string): Date | null {
  const match = /^(\d{4})-W(\d{2})$/.exec(key);
  if (!match) return null;
  const year = Number(match[1]);
  const week = Number(match[2]);
  if (week < 1 || week > 53) return null;

  // Jan 4th is always in ISO week 1. Find that week's Monday, then offset.
  const jan4 = new Date(year, 0, 4);
  const jan4Dow = (jan4.getDay() + 6) % 7; // Monday=0 .. Sunday=6
  const week1Monday = new Date(year, 0, 4 - jan4Dow);
  const monday = new Date(week1Monday);
  monday.setDate(week1Monday.getDate() + (week - 1) * 7);
  return monday;
}

/** ISO 8601 week token "YYYY-Www" for a given local date (inverse of
 *  `isoWeekKeyToMondayDate` at the resolution of a week). The ISO year is
 *  defined by the week's Thursday, so a late-December date can land in W01
 *  of the next year and an early-January date in W52/W53 of the previous. */
export function isoWeekOfDate(date: Date): string {
  const target = new Date(date.getFullYear(), date.getMonth(), date.getDate());
  const dayNr = (target.getDay() + 6) % 7; // Monday=0
  target.setDate(target.getDate() - dayNr + 3); // this week's Thursday
  const isoYear = target.getFullYear();
  const firstThursday = new Date(isoYear, 0, 4);
  const firstDayNr = (firstThursday.getDay() + 6) % 7;
  firstThursday.setDate(firstThursday.getDate() - firstDayNr + 3);
  const week =
    1 + Math.round((target.getTime() - firstThursday.getTime()) / (7 * 24 * 3600 * 1000));
  return `${isoYear}-W${pad2(week)}`;
}

/** Local "YYYY-MM-DD" for a Date. */
export function dayKey(d: Date): string {
  return `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`;
}

/** Epoch ms -> "YYYY-MM-DD" in local TZ (inverse of dateInput's parse). */
export function epochMsToDailyKey(ms: number): string {
  return dayKey(new Date(ms));
}

/** Local year+month -> "YYYY-MM" month key. */
export function yearMonthToKey(year: number, month: number): string {
  return `${year}-${pad2(month)}`;
}

/**
 * Classify a period_key from the workflow's `projectHits` projection into
 * the calendar `SummaryKind` it belongs to, and return the matching
 * `month` (the "YYYY-MM" key the Summaries calendar tab pages on).
 *
 * Token formats (defined by the summaries pipeline `externalId` prefix
 * strip — see `workflows/rag/lookback-recall.yaml::projectHits`):
 *   - daily:   "YYYY-MM-DD"
 *   - weekly:  "YYYY-Www"     (ISO 8601 week)
 *   - monthly: "YYYY-MM"
 *
 * Returns null when the token doesn't match any of the three shapes — the
 * caller should fall back to a plain tab switch in that case so the user
 * isn't dropped into an empty month.
 */
export function classifyPeriodKey(
  periodKey: string,
): { kind: "daily" | "weekly" | "monthly"; month: string } | null {
  // Daily — YYYY-MM-DD. Validate the month component so an invalid
  // calendar date like "2026-13-01" falls through.
  const daily = /^(\d{4})-(\d{2})-(\d{2})$/.exec(periodKey);
  if (daily) {
    const ym = monthKeyToYearMonth(`${daily[1]}-${daily[2]}`);
    if (ym) return { kind: "daily", month: yearMonthToKey(ym.y, ym.m) };
  }
  // Weekly — YYYY-Www (Monday of that ISO week determines the month).
  if (/^\d{4}-W\d{2}$/.test(periodKey)) {
    const monday = isoWeekKeyToMondayDate(periodKey);
    if (monday) {
      return { kind: "weekly", month: yearMonthToKey(monday.getFullYear(), monday.getMonth() + 1) };
    }
  }
  // Monthly — YYYY-MM. Reuse `monthKeyToYearMonth` for the range check
  // (it rejects month 00 / 13+).
  const month = monthKeyToYearMonth(periodKey);
  if (month) return { kind: "monthly", month: yearMonthToKey(month.y, month.m) };
  return null;
}

function pad2(n: number): string {
  return String(n).padStart(2, "0");
}

/**
 * Month grid for the calendar: leading nulls pad to the Monday-based weekday
 * of the 1st, then one Date per day of the month. `month` is 1-based.
 */
export function buildMonthGrid(year: number, month: number): Array<Date | null> {
  const first = new Date(year, month - 1, 1);
  const leading = (first.getDay() + 6) % 7; // Monday=0
  const daysInMonth = new Date(year, month, 0).getDate();
  const cells: Array<Date | null> = [];
  for (let i = 0; i < leading; i++) cells.push(null);
  for (let d = 1; d <= daysInMonth; d++) cells.push(new Date(year, month - 1, d));
  return cells;
}

// ---- Staged-generate range expansion --------------------------------------
//
// The dialog picks a range in the top-most selected granularity's unit; to
// feed the dependency chain it is expanded to a `[fromDate, toDate]` day span
// and re-derived into each layer's token. Pure date-string math (no epoch
// round-trip) keeps it TZ/DST-independent, matching the period batches' UTC
// range expansion.

/** A resolved calendar-date span (both inclusive, "YYYY-MM-DD"). */
export interface DayRange {
  fromDate: string;
  toDate: string;
}

/**
 * Expand a range expressed in `topKind`'s unit into an inclusive
 * `[fromDate, toDate]` calendar span:
 *   - monthly: from = month's 1st, to = month's last day.
 *   - weekly:  from = ISO week's Monday, to = that week's Sunday.
 *   - daily / per-thread: the dates pass through unchanged.
 * Returns null when either end is empty or malformed (caller then either
 * sends unbounded for per-thread, or uses `fallbackDayRange`).
 */
export function expandToDayRange(topKind: SummaryKind, from: string, to: string): DayRange | null {
  if (!from || !to) return null;
  switch (topKind) {
    case "monthly": {
      const start = monthKeyToYearMonth(from);
      const end = monthKeyToYearMonth(to);
      if (!start || !end) return null;
      return {
        fromDate: `${yearMonthToKey(start.y, start.m)}-01`,
        // Day 0 of the next month = last day of this month.
        toDate: dayKey(new Date(end.y, end.m, 0)),
      };
    }
    case "weekly": {
      const monday = isoWeekKeyToMondayDate(from);
      const endMonday = isoWeekKeyToMondayDate(to);
      if (!monday || !endMonday) return null;
      const sunday = new Date(endMonday);
      sunday.setDate(endMonday.getDate() + 6);
      return { fromDate: dayKey(monday), toDate: dayKey(sunday) };
    }
    default:
      // daily / per-thread: the picker already yields YYYY-MM-DD.
      return { fromDate: from, toDate: to };
  }
}

/**
 * The fallback span when no range is given but a period layer is selected:
 * mirror each period batch's own fallback so the dependency chain still has
 * source data.
 *   - daily:   yesterday.
 *   - weekly:  the previous completed week (Mon–Sun).
 *   - monthly: the previous completed month (1st–last).
 * per-thread is intentionally NOT handled here — a per-thread-only run with no
 * range stays unbounded (the dialog handles that), preserving the recovery path.
 *
 * "Now" is taken in `tzOffsetHours`, NOT the browser's local zone: the period
 * batches resolve their own fallback as `now_utc + offset` (e.g.
 * daily-work-summary-batch's `yesterday_epoch`). Computing it in the browser's
 * zone would, near UTC midnight or when the configured offset differs from the
 * browser (e.g. resolveTimezoneOffsetHours falling back to JST), target a
 * different previous period than the workflow would have. `now` is injectable
 * for tests.
 */
export function fallbackDayRange(
  topKind: SummaryKind,
  tzOffsetHours: number,
  now: Date = new Date(),
): DayRange | null {
  // Shift to the offset's wall clock, then read the shifted UTC fields back
  // into a local Date so the y/m/d arithmetic below runs in the target zone.
  const shifted = new Date(now.getTime() + tzOffsetHours * 3_600_000);
  const wall = new Date(shifted.getUTCFullYear(), shifted.getUTCMonth(), shifted.getUTCDate());
  switch (topKind) {
    case "daily": {
      wall.setDate(wall.getDate() - 1);
      const key = dayKey(wall);
      return { fromDate: key, toDate: key };
    }
    case "weekly": {
      // Last week = this week's Monday stepped back 7 days, Mon..Sun.
      const lastMonday = toMonday(wall);
      lastMonday.setDate(lastMonday.getDate() - 7);
      const lastSunday = new Date(lastMonday);
      lastSunday.setDate(lastMonday.getDate() + 6);
      return { fromDate: dayKey(lastMonday), toDate: dayKey(lastSunday) };
    }
    case "monthly": {
      // 1st of last month .. last day of last month.
      const lastMonthEnd = new Date(wall.getFullYear(), wall.getMonth(), 0);
      const firstLast = new Date(lastMonthEnd.getFullYear(), lastMonthEnd.getMonth(), 1);
      return { fromDate: dayKey(firstLast), toDate: dayKey(lastMonthEnd) };
    }
    default:
      return null;
  }
}

/** Pull `fromDate` back to its ISO week's Monday, leaving `toDate` untouched.
 *
 *  A weekly summary reads every daily in `[Monday, Sunday]`, so a month/week
 *  range whose START falls mid-week would build its first week from only the
 *  in-range days. Extending the start to the week's Monday fixes that.
 *
 *  Crucially the END is NOT extended to the following Sunday: a weekly memory's
 *  updated_at is `max(daily.updated_at)`, and monthly aggregation attributes a
 *  week to the month of that max. Extending a month-end week into the next
 *  month would push its updated_at past month_end, dropping the week (and its
 *  in-month days) from this month's monthly summary. Keeping the end at the
 *  selected boundary leaves the trailing week attributed to — and aggregated
 *  by — the correct month, built from its in-month days. */
export function extendStartToWeekStart(range: DayRange): DayRange {
  return { fromDate: dayKey(toMonday(localDate(range.fromDate))), toDate: range.toDate };
}

/** Per-layer period tokens for the pipeline input. The weekly/monthly tokens
 *  come from `periodSpan` (the user's selected range), while the daily token
 *  comes from `dailySpan` (start-extended for weekly/monthly runs so the
 *  leading boundary week gets all its days). Pure string math; epoch-free,
 *  TZ/DST-independent. */
export interface PeriodTokens {
  daily_start: string;
  daily_end: string;
  weekly_start: string;
  weekly_end: string;
  monthly_start: string;
  monthly_end: string;
}

export function dayRangeToPeriodTokens(periodSpan: DayRange, dailySpan: DayRange): PeriodTokens {
  return {
    daily_start: dailySpan.fromDate,
    daily_end: dailySpan.toDate,
    weekly_start: isoWeekOfDate(localDate(periodSpan.fromDate)),
    weekly_end: isoWeekOfDate(localDate(periodSpan.toDate)),
    monthly_start: periodSpan.fromDate.slice(0, 7),
    monthly_end: periodSpan.toDate.slice(0, 7),
  };
}

/** "YYYY-MM-DD" -> a local-TZ Date at that calendar day (no epoch round-trip,
 *  so DST/offset can't shift it — `isoWeekOfDate` only reads the y/m/d). */
function localDate(yyyymmdd: string): Date {
  const [y, m, d] = yyyymmdd.split("-").map(Number);
  return new Date(y ?? 1970, (m ?? 1) - 1, d ?? 1);
}

/** New Date pulled back to its ISO week's Monday (Monday-based weekday). */
function toMonday(d: Date): Date {
  const monday = new Date(d);
  monday.setDate(d.getDate() - ((d.getDay() + 6) % 7));
  return monday;
}
