import { afterEach, describe, expect, it, vi } from "vitest";
import {
  dayRangeToEpochMs,
  localDateToEpochMs,
  localDateToIsoUtc,
  localTodayMinusDays,
  resolveTimezoneOffsetHours,
} from "./dateInput";

const DAY_MS = 86_400_000;

describe("localDateToEpochMs", () => {
  it("anchors the date to local-TZ midnight (not UTC)", () => {
    // Independent oracle: a local-midnight epoch ms must render back to
    // the same calendar date at 00:00 in the local TZ. This catches a
    // month/day off-by-one without re-implementing the helper.
    const ms = localDateToEpochMs("2026-05-01");
    expect(ms).toBeDefined();
    const back = new Date(ms as number);
    expect(back.getFullYear()).toBe(2026);
    expect(back.getMonth()).toBe(4); // May (0-indexed)
    expect(back.getDate()).toBe(1);
    expect(back.getHours()).toBe(0);
    expect(back.getMinutes()).toBe(0);
    expect(back.getSeconds()).toBe(0);
    expect(back.getMilliseconds()).toBe(0);
  });

  it("advances by exactly one day between consecutive dates", () => {
    // TZ-independent invariant: outside DST transitions, adjacent
    // calendar days are 24h apart. JST/UTC (the supported runtime) have
    // no DST, so this holds.
    const a = localDateToEpochMs("2026-05-01");
    const b = localDateToEpochMs("2026-05-02");
    expect(a).toBeDefined();
    expect(b).toBeDefined();
    expect((b as number) - (a as number)).toBe(DAY_MS);
  });

  it("returns undefined for empty or malformed input", () => {
    expect(localDateToEpochMs("")).toBeUndefined();
    expect(localDateToEpochMs("bad")).toBeUndefined();
    expect(localDateToEpochMs("2026-05")).toBeUndefined();
  });

  it("rejects out-of-range parts instead of silently rolling over", () => {
    // The Date constructor would roll month 13 -> next Jan and Feb 31 ->
    // Mar; both must be rejected to keep the filter on the chosen day.
    expect(localDateToEpochMs("2026-13-01")).toBeUndefined();
    expect(localDateToEpochMs("2026-05-40")).toBeUndefined();
    expect(localDateToEpochMs("2026-02-31")).toBeUndefined();
  });
});

describe("localDateToIsoUtc", () => {
  it("round-trips through local midnight epoch ms", () => {
    const ms = localDateToEpochMs("2026-05-01");
    expect(localDateToIsoUtc("2026-05-01")).toBe(new Date(ms as number).toISOString());
  });

  it("returns undefined for malformed input", () => {
    expect(localDateToIsoUtc("")).toBeUndefined();
  });
});

describe("localTodayMinusDays", () => {
  it("returns today's local calendar date for 0", () => {
    const now = new Date();
    const expected = `${now.getFullYear()}-${String(now.getMonth() + 1).padStart(2, "0")}-${String(now.getDate()).padStart(2, "0")}`;
    expect(localTodayMinusDays(0)).toBe(expected);
  });

  it("yields a parseable date one day apart per step", () => {
    const today = localDateToEpochMs(localTodayMinusDays(0));
    const yesterday = localDateToEpochMs(localTodayMinusDays(1));
    expect(today).toBeDefined();
    expect(yesterday).toBeDefined();
    expect((today as number) - (yesterday as number)).toBe(DAY_MS);
  });
});

describe("dayRangeToEpochMs", () => {
  it("returns both-undefined when both ends are empty (unbounded)", () => {
    expect(dayRangeToEpochMs()).toEqual({ after: undefined, before: undefined });
    expect(dayRangeToEpochMs("", "")).toEqual({ after: undefined, before: undefined });
  });

  it("nudges after to from-00:00 minus 1ms (strict > keeps the from day)", () => {
    const fromMs = localDateToEpochMs("2026-05-01") as number;
    const { after } = dayRangeToEpochMs("2026-05-01", undefined);
    expect(after).toBe(fromMs - 1);
  });

  it("nudges before to the next-day-00:00 minus 1ms (inclusive <= keeps the to day)", () => {
    const nextDayMs = localDateToEpochMs("2026-06-01") as number;
    const { before } = dayRangeToEpochMs(undefined, "2026-05-31");
    // to=2026-05-31 -> end is 2026-06-01 00:00 - 1ms = 23:59:59.999 on the 31st.
    expect(before).toBe(nextDayMs - 1);
  });

  it("supports each end independently", () => {
    const onlyFrom = dayRangeToEpochMs("2026-05-01", "");
    expect(onlyFrom.after).toBeDefined();
    expect(onlyFrom.before).toBeUndefined();
    const onlyTo = dayRangeToEpochMs("", "2026-05-31");
    expect(onlyTo.after).toBeUndefined();
    expect(onlyTo.before).toBeDefined();
  });
});

describe("resolveTimezoneOffsetHours", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("uses the integer system offset when whole hours (JST +9)", () => {
    // getTimezoneOffset returns minutes WEST of UTC; JST is -540.
    vi.spyOn(Date.prototype, "getTimezoneOffset").mockReturnValue(-540);
    expect(resolveTimezoneOffsetHours()).toBe(9);
  });

  it("falls back for fractional zones (+5:30)", () => {
    vi.spyOn(Date.prototype, "getTimezoneOffset").mockReturnValue(-330);
    expect(resolveTimezoneOffsetHours()).toBe(9); // default fallback
    expect(resolveTimezoneOffsetHours(undefined, 0)).toBe(0);
  });

  it("falls back for negative offsets (the Americas, e.g. -5)", () => {
    vi.spyOn(Date.prototype, "getTimezoneOffset").mockReturnValue(300);
    // -5 is outside the 0..23 range the workflow accepts -> fallback.
    expect(resolveTimezoneOffsetHours()).toBe(9);
  });

  it("honours a valid integer override over the system value", () => {
    vi.spyOn(Date.prototype, "getTimezoneOffset").mockReturnValue(-540);
    expect(resolveTimezoneOffsetHours(0)).toBe(0);
    // Out-of-range override is ignored in favour of the system value.
    expect(resolveTimezoneOffsetHours(99)).toBe(9);
  });
});
