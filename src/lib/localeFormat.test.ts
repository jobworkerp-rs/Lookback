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

  it("formats numbers with the effective locale and handles boundaries", () => {
    expect(formatNumber(1234567, "ja")).toBe((1234567).toLocaleString("ja"));
    expect(formatNumber(1234567, "en")).toBe((1234567).toLocaleString("en"));
    expect(formatNumber(0, undefined)).toBe((0).toLocaleString("en"));
  });
});
