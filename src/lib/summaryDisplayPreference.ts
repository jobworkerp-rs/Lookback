// Persistence for the period-summary reading preference. This stays in
// localStorage because it affects only this device's UI and is not user data.

export type PeriodSummaryExpansionMode = "collapsed" | "expanded";

export interface PeriodSummaryDisplayPreference {
  remember: boolean;
  mode: PeriodSummaryExpansionMode;
}

export const PERIOD_SUMMARY_DISPLAY_PREFERENCE_STORAGE_KEY = "lookback.periodSummaryDisplay";

export const DEFAULT_PERIOD_SUMMARY_DISPLAY_PREFERENCE: PeriodSummaryDisplayPreference = {
  remember: false,
  mode: "collapsed",
};

function isPreference(value: unknown): value is PeriodSummaryDisplayPreference {
  if (value == null || typeof value !== "object") return false;
  const candidate = value as Record<string, unknown>;
  return (
    typeof candidate.remember === "boolean" &&
    (candidate.mode === "collapsed" || candidate.mode === "expanded")
  );
}

export function loadPeriodSummaryDisplayPreference(): PeriodSummaryDisplayPreference {
  try {
    const raw = localStorage.getItem(PERIOD_SUMMARY_DISPLAY_PREFERENCE_STORAGE_KEY);
    if (raw == null) return DEFAULT_PERIOD_SUMMARY_DISPLAY_PREFERENCE;
    const parsed: unknown = JSON.parse(raw);
    return isPreference(parsed) ? parsed : DEFAULT_PERIOD_SUMMARY_DISPLAY_PREFERENCE;
  } catch {
    return DEFAULT_PERIOD_SUMMARY_DISPLAY_PREFERENCE;
  }
}

export function savePeriodSummaryDisplayPreference(
  preference: PeriodSummaryDisplayPreference,
): void {
  try {
    localStorage.setItem(PERIOD_SUMMARY_DISPLAY_PREFERENCE_STORAGE_KEY, JSON.stringify(preference));
  } catch {
    // A storage failure only prevents restoring the visual preference.
  }
}
