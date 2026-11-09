import { afterEach, describe, expect, it } from "vitest";
import {
  DEFAULT_PERIOD_SUMMARY_DISPLAY_PREFERENCE,
  loadPeriodSummaryDisplayPreference,
  PERIOD_SUMMARY_DISPLAY_PREFERENCE_STORAGE_KEY,
  savePeriodSummaryDisplayPreference,
} from "./summaryDisplayPreference";

afterEach(() => {
  localStorage.clear();
});

describe("period summary display preference", () => {
  it("uses collapsed, non-persistent display when no preference is stored", () => {
    expect(loadPeriodSummaryDisplayPreference()).toEqual(DEFAULT_PERIOD_SUMMARY_DISPLAY_PREFERENCE);
  });

  it("ignores malformed persisted values", () => {
    localStorage.setItem(PERIOD_SUMMARY_DISPLAY_PREFERENCE_STORAGE_KEY, '{"remember":true}');
    expect(loadPeriodSummaryDisplayPreference()).toEqual(DEFAULT_PERIOD_SUMMARY_DISPLAY_PREFERENCE);
  });

  it("round-trips whether the default expansion mode should be remembered", () => {
    const preference = { remember: true, mode: "expanded" as const };
    savePeriodSummaryDisplayPreference(preference);
    expect(loadPeriodSummaryDisplayPreference()).toEqual(preference);
  });
});
