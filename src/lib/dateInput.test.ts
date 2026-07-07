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

  it("anchors to midnight in the given IANA timezone, not the host zone", () => {
    // Independent oracle: the returned epoch, rendered back in the SAME zone,
    // must read 2026-05-01 00:00:00. Asia/Tokyo (UTC+9, no DST) => the UTC
    // instant is 2026-04-30T15:00:00Z.
    const tokyo = localDateToEpochMs("2026-05-01", "Asia/Tokyo");
    expect(tokyo).toBe(Date.UTC(2026, 3, 30, 15, 0, 0));

    // America/New_York on 2026-05-01 is EDT (UTC-4) => 2026-05-01T04:00:00Z.
    const ny = localDateToEpochMs("2026-05-01", "America/New_York");
    expect(ny).toBe(Date.UTC(2026, 4, 1, 4, 0, 0));

    // Different zones for the same calendar date resolve to different instants.
    expect(tokyo).not.toBe(ny);
  });

  it("handles a zone whose midnight offset differs from a mid-year offset (DST)", () => {
    // America/New_York on 2026-01-01 is EST (UTC-5) => 2026-01-01T05:00:00Z.
    // Verifies the offset is taken AT the target date, not a fixed value.
    expect(localDateToEpochMs("2026-01-01", "America/New_York")).toBe(
      Date.UTC(2026, 0, 1, 5, 0, 0),
    );
  });

  it("handles zones where DST starts at local midnight", () => {
    // Africa/Cairo advanced clocks at local 00:00 on 2023-04-28. The first
    // instant in that calendar date is the transition instant, 2023-04-27T22Z.
    expect(localDateToEpochMs("2023-04-28", "Africa/Cairo")).toBe(Date.UTC(2023, 3, 27, 22, 0, 0));
  });

  it("keeps west-of-UTC midnight DST gaps on the requested date", () => {
    // America/Santiago skipped local 00:00 on 2024-09-08. The first real
    // instant in that date is 01:00 CLST, i.e. 2024-09-08T04:00:00Z.
    expect(localDateToEpochMs("2024-09-08", "America/Santiago")).toBe(
      Date.UTC(2024, 8, 8, 4, 0, 0),
    );
  });

  it("falls back to the host zone for a blank or invalid timezone", () => {
    expect(localDateToEpochMs("2026-05-01", "")).toBe(localDateToEpochMs("2026-05-01"));
    expect(localDateToEpochMs("2026-05-01", "   ")).toBe(localDateToEpochMs("2026-05-01"));
    expect(localDateToEpochMs("2026-05-01", "Not/AZone")).toBe(localDateToEpochMs("2026-05-01"));
  });

  it("rejects out-of-range parts even with a timezone", () => {
    expect(localDateToEpochMs("2026-02-31", "Asia/Tokyo")).toBeUndefined();
    expect(localDateToEpochMs("2026-13-01", "America/New_York")).toBeUndefined();
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

  it("does not roll an invalid to-date into a different boundary", () => {
    expect(dayRangeToEpochMs(undefined, "2026-02-31")).toEqual({
      after: undefined,
      before: undefined,
    });
    expect(dayRangeToEpochMs("2026-05-01", "2026-13-01", "Asia/Tokyo")).toEqual({
      after: (localDateToEpochMs("2026-05-01", "Asia/Tokyo") as number) - 1,
      before: undefined,
    });
  });

  it("anchors both ends to the given timezone", () => {
    // Range boundaries must be computed in the display zone so they match the
    // rendered timestamps. from=2026-05-01, to=2026-05-31 in Asia/Tokyo:
    //   after  = 2026-05-01 00:00 JST − 1ms
    //   before = 2026-06-01 00:00 JST − 1ms
    const { after, before } = dayRangeToEpochMs("2026-05-01", "2026-05-31", "Asia/Tokyo");
    expect(after).toBe((localDateToEpochMs("2026-05-01", "Asia/Tokyo") as number) - 1);
    expect(before).toBe((localDateToEpochMs("2026-06-01", "Asia/Tokyo") as number) - 1);
    // A different zone shifts both ends by its offset delta.
    const ny = dayRangeToEpochMs("2026-05-01", "2026-05-31", "America/New_York");
    expect(ny.after).not.toBe(after);
  });

  it("keeps range ends valid when the next day starts at a DST transition", () => {
    const { after, before } = dayRangeToEpochMs("2023-04-27", "2023-04-27", "Africa/Cairo");
    expect(after).toBe((localDateToEpochMs("2023-04-27", "Africa/Cairo") as number) - 1);
    expect(before).toBe(Date.UTC(2023, 3, 27, 22, 0, 0) - 1);
  });

  it("keeps west-of-UTC midnight DST gap range starts on the requested date", () => {
    const { after, before } = dayRangeToEpochMs("2024-09-08", "2024-09-08", "America/Santiago");
    expect(after).toBe(Date.UTC(2024, 8, 8, 4, 0, 0) - 1);
    expect(before).toBe(Date.UTC(2024, 8, 9, 3, 0, 0) - 1);
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

  it("derives the whole-hour offset from an IANA timezone, not the OS", () => {
    // OS is UTC (offset 0) so the OS fallback can't masquerade as the zone.
    vi.spyOn(Date.prototype, "getTimezoneOffset").mockReturnValue(0);

    // Asia/Tokyo via the zone path yields +9 — sourced from the zone, since
    // the OS fallback would give 0.
    expect(resolveTimezoneOffsetHours(undefined, 5, "Asia/Tokyo")).toBe(9);

    // Asia/Kolkata is +5:30 (fractional): the zone path can't produce a whole
    // hour, so it falls through to the OS offset (0 here).
    expect(resolveTimezoneOffsetHours(undefined, 5, "Asia/Kolkata")).toBe(0);

    // A west-of-UTC zone is a negative offset, out of the 0..23 range the
    // workflow accepts, so it falls through to the OS offset (0 here).
    expect(resolveTimezoneOffsetHours(undefined, 5, "America/New_York")).toBe(0);
  });

  it("prefers an explicit override over the timezone argument", () => {
    expect(resolveTimezoneOffsetHours(3, 9, "Asia/Tokyo")).toBe(3);
  });
});

describe("localTodayMinusDays with a timezone", () => {
  it("returns today's calendar date in the given zone", () => {
    // Independent oracle: format now in the zone and compare the date part.
    const zoneToday = new Intl.DateTimeFormat("en-CA", {
      timeZone: "America/New_York",
      year: "numeric",
      month: "2-digit",
      day: "2-digit",
    }).format(new Date());
    expect(localTodayMinusDays(0, "America/New_York")).toBe(zoneToday);
  });

  it("steps back exactly one calendar day in the zone", () => {
    const today = localDateToEpochMs(localTodayMinusDays(0, "Asia/Tokyo"), "Asia/Tokyo");
    const yesterday = localDateToEpochMs(localTodayMinusDays(1, "Asia/Tokyo"), "Asia/Tokyo");
    expect((today as number) - (yesterday as number)).toBe(DAY_MS);
  });
});
