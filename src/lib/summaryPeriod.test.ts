import { describe, expect, it } from "vitest";
import {
  buildMonthGrid,
  classifyPeriodKey,
  dayRangeToPeriodTokens,
  epochMsToDailyKey,
  expandToDayRange,
  extendStartToWeekStart,
  fallbackDayRange,
  isoWeekKeyToMondayDate,
  isoWeekOfDate,
  monthKeyToYearMonth,
  yearMonthToKey,
} from "./summaryPeriod";

describe("monthKeyToYearMonth", () => {
  it("parses a well-formed month key", () => {
    expect(monthKeyToYearMonth("2026-05")).toEqual({ y: 2026, m: 5 });
  });

  it("rejects malformed keys", () => {
    expect(monthKeyToYearMonth("2026-13")).toBeNull();
    expect(monthKeyToYearMonth("2026-05-24")).toBeNull();
    expect(monthKeyToYearMonth("2026")).toBeNull();
  });
});

describe("isoWeekKeyToMondayDate", () => {
  it("maps a mid-year week to its Monday", () => {
    // 2026-W21 starts Monday 2026-05-18.
    const d = isoWeekKeyToMondayDate("2026-W21");
    expect(d).not.toBeNull();
    expect(d?.getFullYear()).toBe(2026);
    expect(d?.getMonth()).toBe(4); // May (0-based)
    expect(d?.getDate()).toBe(18);
  });

  it("handles the year-boundary W01 (Monday in the previous year)", () => {
    // ISO 2026-W01 starts Monday 2025-12-29.
    const d = isoWeekKeyToMondayDate("2026-W01");
    expect(d?.getFullYear()).toBe(2025);
    expect(d?.getMonth()).toBe(11); // December
    expect(d?.getDate()).toBe(29);
  });

  it("handles a leap week year W53", () => {
    // ISO 2020-W53 starts Monday 2020-12-28.
    const d = isoWeekKeyToMondayDate("2020-W53");
    expect(d?.getFullYear()).toBe(2020);
    expect(d?.getMonth()).toBe(11);
    expect(d?.getDate()).toBe(28);
  });

  it("rejects malformed week keys", () => {
    expect(isoWeekKeyToMondayDate("2026-21")).toBeNull();
    expect(isoWeekKeyToMondayDate("2026-W00")).toBeNull();
    expect(isoWeekKeyToMondayDate("2026-W54")).toBeNull();
    expect(isoWeekKeyToMondayDate("2026-05-24")).toBeNull();
  });
});

describe("isoWeekOfDate", () => {
  it("returns the ISO week of a mid-year date", () => {
    // 2026-05-18 is Monday of W21.
    expect(isoWeekOfDate(new Date(2026, 4, 18))).toBe("2026-W21");
    expect(isoWeekOfDate(new Date(2026, 4, 24))).toBe("2026-W21"); // Sunday of W21
  });

  it("rolls a late-December date into W01 of the next ISO year", () => {
    // 2025-12-29 is Monday of ISO 2026-W01.
    expect(isoWeekOfDate(new Date(2025, 11, 29))).toBe("2026-W01");
  });

  it("is the inverse of isoWeekKeyToMondayDate at week resolution", () => {
    for (const key of ["2026-W01", "2026-W21", "2020-W53"]) {
      const monday = isoWeekKeyToMondayDate(key);
      expect(monday).not.toBeNull();
      if (monday) expect(isoWeekOfDate(monday)).toBe(key);
    }
  });
});

describe("epochMsToDailyKey", () => {
  it("round-trips a local date through localDateToEpochMs", () => {
    // Build local midnight for 2026-05-24 and convert back.
    const ms = new Date(2026, 4, 24).getTime();
    expect(epochMsToDailyKey(ms)).toBe("2026-05-24");
  });

  it("zero-pads month and day", () => {
    const ms = new Date(2026, 0, 3).getTime();
    expect(epochMsToDailyKey(ms)).toBe("2026-01-03");
  });
});

describe("yearMonthToKey", () => {
  it("zero-pads the month", () => {
    expect(yearMonthToKey(2026, 5)).toBe("2026-05");
    expect(yearMonthToKey(2026, 12)).toBe("2026-12");
  });
});

describe("buildMonthGrid", () => {
  it("pads leading blanks to the Monday-based weekday of the 1st", () => {
    // May 2026: the 1st is a Friday. Monday-based index = 4 leading blanks.
    const grid = buildMonthGrid(2026, 5);
    expect(grid.slice(0, 4)).toEqual([null, null, null, null]);
    expect(grid[4]).not.toBeNull();
    expect(grid[4]?.getDate()).toBe(1);
  });

  it("contains exactly the days of the month as non-null cells", () => {
    const grid = buildMonthGrid(2026, 2); // Feb 2026 has 28 days
    const days = grid.filter((c): c is Date => c !== null);
    expect(days.length).toBe(28);
    expect(days[0]?.getDate()).toBe(1);
    expect(days[days.length - 1]?.getDate()).toBe(28);
  });

  it("handles a leap February", () => {
    const grid = buildMonthGrid(2028, 2); // 2028 is a leap year
    const days = grid.filter((c): c is Date => c !== null);
    expect(days.length).toBe(29);
  });
});

describe("expandToDayRange", () => {
  it("expands a monthly range to month-start..month-end", () => {
    expect(expandToDayRange("monthly", "2026-03", "2026-05")).toEqual({
      fromDate: "2026-03-01",
      toDate: "2026-05-31",
    });
  });

  it("expands a single-month range and respects month length (Feb)", () => {
    expect(expandToDayRange("monthly", "2026-02", "2026-02")).toEqual({
      fromDate: "2026-02-01",
      toDate: "2026-02-28",
    });
  });

  it("expands a weekly range to Monday..Sunday", () => {
    // 2026-W21 Monday = 2026-05-18; W22 Sunday = 2026-05-31.
    expect(expandToDayRange("weekly", "2026-W21", "2026-W22")).toEqual({
      fromDate: "2026-05-18",
      toDate: "2026-05-31",
    });
  });

  it("passes daily/per-thread dates through unchanged", () => {
    expect(expandToDayRange("daily", "2026-05-01", "2026-05-31")).toEqual({
      fromDate: "2026-05-01",
      toDate: "2026-05-31",
    });
    expect(expandToDayRange("per-thread", "2026-05-01", "2026-05-31")).toEqual({
      fromDate: "2026-05-01",
      toDate: "2026-05-31",
    });
  });

  it("returns null for empty or malformed input", () => {
    expect(expandToDayRange("monthly", "", "2026-05")).toBeNull();
    expect(expandToDayRange("monthly", "2026-13", "2026-05")).toBeNull();
    expect(expandToDayRange("weekly", "2026-W21", "2026-W99")).toBeNull();
  });
});

describe("fallbackDayRange", () => {
  // Noon UTC keeps `wall` on 2026-05-24 for any sane offset, so these assert
  // the period logic independent of the test runner's own zone.
  const noonUtc = new Date(Date.UTC(2026, 4, 24, 12, 0, 0));

  it("daily -> yesterday (in the workflow tz)", () => {
    expect(fallbackDayRange("daily", 9, noonUtc)).toEqual({
      fromDate: "2026-05-23",
      toDate: "2026-05-23",
    });
  });

  it("weekly -> previous completed week (Mon..Sun)", () => {
    // 2026-05-24 is a Sunday (W21). Last completed week = W20: 2026-05-11..05-17.
    expect(fallbackDayRange("weekly", 9, noonUtc)).toEqual({
      fromDate: "2026-05-11",
      toDate: "2026-05-17",
    });
  });

  it("monthly -> previous completed month (1st..last)", () => {
    expect(fallbackDayRange("monthly", 9, noonUtc)).toEqual({
      fromDate: "2026-04-01",
      toDate: "2026-04-30",
    });
  });

  it("resolves 'now' in the workflow tz, not the browser zone", () => {
    // At 02:00 UTC the offset decides which calendar day "today" is, so the
    // fallback's yesterday must follow the offset — matching the batch's own
    // `now_utc + offset` fallback rather than the browser's local midnight.
    const earlyUtc = new Date(Date.UTC(2026, 4, 24, 2, 0, 0));
    // offset +9: wall = 2026-05-24 -> yesterday 05-23.
    expect(fallbackDayRange("daily", 9, earlyUtc)?.fromDate).toBe("2026-05-23");
    // offset -5: wall = 2026-05-23 -> yesterday 05-22 (one day earlier).
    expect(fallbackDayRange("daily", -5, earlyUtc)?.fromDate).toBe("2026-05-22");
  });

  it("returns null for per-thread (handled as unbounded elsewhere)", () => {
    expect(fallbackDayRange("per-thread", 9)).toBeNull();
  });
});

describe("extendStartToWeekStart", () => {
  it("pulls a mid-week start back to its Monday", () => {
    // 2026-05-01 (Fri) is in W18, whose Monday is 2026-04-27.
    expect(extendStartToWeekStart({ fromDate: "2026-05-01", toDate: "2026-05-31" })).toEqual({
      fromDate: "2026-04-27",
      toDate: "2026-05-31",
    });
  });

  it("leaves the END untouched even when it falls mid-week", () => {
    // 2026-04-30 (Thu) must NOT extend to the next Sunday (5/3): doing so would
    // push W18's updated_at into May and drop it from April's monthly summary.
    expect(extendStartToWeekStart({ fromDate: "2026-04-01", toDate: "2026-04-30" })).toEqual({
      fromDate: "2026-03-30", // 2026-04-01 (Wed, W14) Monday
      toDate: "2026-04-30", // unchanged
    });
  });

  it("is a no-op when the start already sits on a Monday", () => {
    expect(extendStartToWeekStart({ fromDate: "2026-05-18", toDate: "2026-05-31" })).toEqual({
      fromDate: "2026-05-18",
      toDate: "2026-05-31",
    });
  });

  it("crosses a year boundary when extending the start", () => {
    // 2026-01-01 (Thu, W01) Monday = 2025-12-29.
    expect(extendStartToWeekStart({ fromDate: "2026-01-01", toDate: "2026-01-31" })).toEqual({
      fromDate: "2025-12-29",
      toDate: "2026-01-31",
    });
  });
});

describe("dayRangeToPeriodTokens", () => {
  it("takes period tokens from periodSpan and daily from dailySpan", () => {
    // Monthly run: period span = 2026-05-01..05-31, daily span start-extended.
    const periodSpan = { fromDate: "2026-05-01", toDate: "2026-05-31" };
    const dailySpan = extendStartToWeekStart(periodSpan);
    const tokens = dayRangeToPeriodTokens(periodSpan, dailySpan);
    // Daily start backs up to the leading week's Monday; end stays the month end.
    expect(tokens.daily_start).toBe("2026-04-27");
    expect(tokens.daily_end).toBe("2026-05-31");
    // Monthly stays the selected month (not the extended 2026-04).
    expect(tokens.monthly_start).toBe("2026-05");
    expect(tokens.monthly_end).toBe("2026-05");
    // Weekly = the period span's endpoints' ISO weeks.
    expect(tokens.weekly_start).toBe("2026-W18");
    expect(tokens.weekly_end).toBe("2026-W22");
  });

  it("month-end run keeps the daily end inside the month (boundary week attribution)", () => {
    // 2026-04: extending only the start keeps daily_end at 04-30, so W18's
    // updated_at stays in April and the month aggregates it.
    const periodSpan = { fromDate: "2026-04-01", toDate: "2026-04-30" };
    const tokens = dayRangeToPeriodTokens(periodSpan, extendStartToWeekStart(periodSpan));
    expect(tokens.daily_start).toBe("2026-03-30");
    expect(tokens.daily_end).toBe("2026-04-30");
    expect(tokens.monthly_start).toBe("2026-04");
    expect(tokens.monthly_end).toBe("2026-04");
  });

  it("daily run uses the same span for both (no widening)", () => {
    const span = { fromDate: "2026-03-01", toDate: "2026-05-31" };
    const tokens = dayRangeToPeriodTokens(span, span);
    expect(tokens.daily_start).toBe("2026-03-01");
    expect(tokens.daily_end).toBe("2026-05-31");
    expect(tokens.monthly_start).toBe("2026-03");
    expect(tokens.monthly_end).toBe("2026-05");
    expect(tokens.weekly_start).toBe("2026-W09");
    expect(tokens.weekly_end).toBe("2026-W22");
  });

  it("handles a year-crossing week endpoint", () => {
    const span = { fromDate: "2025-12-29", toDate: "2025-12-31" };
    const tokens = dayRangeToPeriodTokens(span, span);
    expect(tokens.weekly_start).toBe("2026-W01");
    expect(tokens.weekly_end).toBe("2026-W01");
  });
});

describe("classifyPeriodKey", () => {
  it("recognises monthly keys (YYYY-MM)", () => {
    expect(classifyPeriodKey("2026-05")).toEqual({ kind: "monthly", month: "2026-05" });
  });

  it("recognises daily keys and derives the matching month", () => {
    expect(classifyPeriodKey("2026-05-28")).toEqual({ kind: "daily", month: "2026-05" });
  });

  it("recognises ISO weekly keys and derives the Monday's month", () => {
    // 2026-W22's Monday is 2026-05-25, which lands in May.
    expect(classifyPeriodKey("2026-W22")).toEqual({ kind: "weekly", month: "2026-05" });
  });

  it("handles a year-crossing weekly key (Monday in the next ISO year)", () => {
    // 2026-W01's Monday is 2025-12-29 → December of the previous year.
    expect(classifyPeriodKey("2026-W01")).toEqual({ kind: "weekly", month: "2025-12" });
  });

  it("returns null for malformed tokens", () => {
    expect(classifyPeriodKey("")).toBeNull();
    expect(classifyPeriodKey("garbage")).toBeNull();
    expect(classifyPeriodKey("2026")).toBeNull();
    expect(classifyPeriodKey("2026-13")).toBeNull();
  });
});
