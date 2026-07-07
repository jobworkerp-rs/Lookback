import { describe, expect, it } from "vitest";
import { formatDateTime, formatNumber } from "./localeFormat";

describe("locale-aware formatting", () => {
  it("formats dates with the effective locale instead of the host default", () => {
    const ms = Date.UTC(2026, 0, 2, 3, 4, 5);

    expect(formatDateTime(ms, "ja")).toBe(new Date(ms).toLocaleString("ja"));
    expect(formatDateTime(ms, "en")).toBe(new Date(ms).toLocaleString("en"));
  });

  it("normalizes regional tags and falls back to English for unsupported locales", () => {
    const ms = Date.UTC(2026, 0, 2, 3, 4, 5);

    // ja-JP collapses to "ja"; en-US and unsupported tags collapse to "en".
    expect(formatDateTime(ms, "ja-JP")).toBe(new Date(ms).toLocaleString("ja"));
    expect(formatDateTime(ms, "en-US")).toBe(new Date(ms).toLocaleString("en"));
    expect(formatDateTime(ms, "fr-FR")).toBe(new Date(ms).toLocaleString("en"));
    expect(formatDateTime(ms, undefined)).toBe(new Date(ms).toLocaleString("en"));
  });

  it("renders the instant in the given IANA timezone instead of the host zone", () => {
    // 2026-01-02T03:04:05Z — a fixed instant, so the wall-clock string differs
    // per zone. Tokyo (UTC+9) and New York (UTC-5, no DST in January) must
    // disagree, and each must match a direct toLocaleString with that zone.
    const ms = Date.UTC(2026, 0, 2, 3, 4, 5);

    expect(formatDateTime(ms, "ja", "Asia/Tokyo")).toBe(
      new Date(ms).toLocaleString("ja", { timeZone: "Asia/Tokyo" }),
    );
    expect(formatDateTime(ms, "en", "America/New_York")).toBe(
      new Date(ms).toLocaleString("en", { timeZone: "America/New_York" }),
    );
    expect(formatDateTime(ms, "en", "Asia/Tokyo")).not.toBe(
      formatDateTime(ms, "en", "America/New_York"),
    );
  });

  it("falls back to the host zone for a blank or invalid timezone", () => {
    const ms = Date.UTC(2026, 0, 2, 3, 4, 5);
    const hostZone = new Date(ms).toLocaleString("en");

    // Blank / whitespace / undefined ⇒ host zone (no timeZone option).
    expect(formatDateTime(ms, "en", undefined)).toBe(hostZone);
    expect(formatDateTime(ms, "en", "")).toBe(hostZone);
    expect(formatDateTime(ms, "en", "   ")).toBe(hostZone);
    // A bad IANA name would throw RangeError inside toLocaleString; the helper
    // swallows it and renders in the host zone rather than crashing the list.
    expect(formatDateTime(ms, "en", "Not/AZone")).toBe(hostZone);
  });

  it("formats numbers with the effective locale and handles boundaries", () => {
    expect(formatNumber(1234567, "ja")).toBe((1234567).toLocaleString("ja"));
    expect(formatNumber(1234567, "en")).toBe((1234567).toLocaleString("en"));
    expect(formatNumber(0, undefined)).toBe((0).toLocaleString("en"));
  });
});
