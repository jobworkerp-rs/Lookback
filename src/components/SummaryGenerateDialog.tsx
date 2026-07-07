import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { generateSummaries } from "@/api";
import { DateInput } from "@/components/DateInput";
import { hasLlmInitFailure, type SidecarStatus } from "@/hooks/useSidecarStatus";
import { useTimezone } from "@/hooks/useTimezone";
import { dayRangeToEpochMs, resolveTimezoneOffsetHours } from "@/lib/dateInput";
import {
  applyDependency,
  KIND_LABEL_KEYS,
  KIND_ORDER,
  type KindSelection,
  topKind,
} from "@/lib/summaryKind";
import {
  type DayRange,
  dayRangeToPeriodTokens,
  expandToDayRange,
  extendStartToWeekStart,
  fallbackDayRange,
  type PeriodTokens,
} from "@/lib/summaryPeriod";
import type { GenerateSummariesRequest, SummaryKind } from "@/types/api";
import { Modal } from "./Modal";

export interface SummaryGenerateDialogProps {
  onClose: () => void;
  /** Receives the dispatched job_id so the caller can open a progress slot. */
  onStarted: (jobId: string) => void;
  /** Granularity to preselect — the page mounts this dialog fresh each open,
   *  so it always reflects the page's current tab. */
  initialKind: SummaryKind;
  sidecar: SidecarStatus;
}

/** Native `<input>` type and value format per top-most granularity. */
const PICKER_TYPE: Record<SummaryKind, "date" | "month" | "week"> = {
  "per-thread": "date",
  daily: "date",
  weekly: "week",
  monthly: "month",
};

/** i18n key per granularity for the range hint, resolved with `t` at render. */
const RANGE_HINT_KEY: Record<SummaryKind, string> = {
  "per-thread": "summaryGen.rangeHintPerThread",
  daily: "summaryGen.rangeHintDaily",
  weekly: "summaryGen.rangeHintWeekly",
  monthly: "summaryGen.rangeHintMonthly",
};

const EMPTY_TOKENS: PeriodTokens = {
  daily_start: "",
  daily_end: "",
  weekly_start: "",
  weekly_end: "",
  monthly_start: "",
  monthly_end: "",
};

/** i18n key per build error, resolved with `t` at render. */
const BUILD_ERROR_KEY: Record<BuildError, string> = {
  "one-sided": "summaryGen.errorOneSided",
  reversed: "summaryGen.errorReversed",
  invalid: "summaryGen.errorInvalid",
};

/** Why a build failed — lets the dialog show a specific message instead of
 *  re-deriving the reason from a null result. `null` request + `null` reason =
 *  nothing selected (no message needed). */
export type BuildError = "one-sided" | "reversed" | "invalid";

export interface BuildResult {
  request: GenerateSummariesRequest | null;
  error: BuildError | null;
}

/** Pure builder for the staged-generate request. Exported for unit tests.
 *
 *  `from`/`to` are in the top-most granularity's unit. The range is expanded
 *  to a `[fromDate, toDate]` day span and re-derived into every layer's tokens
 *  + per-thread epoch bounds, so a coarser run also feeds its dependencies.
 *  With no range: per-thread-only stays unbounded (recovery path); otherwise
 *  each layer falls back to its own previous period.
 */
export function buildGenerateRequest(
  selection: KindSelection,
  from: string,
  to: string,
  tzOffsetHours: number,
  timeZone?: string,
): BuildResult {
  const top = topKind(selection);
  if (!top) return { request: null, error: null };

  const base: GenerateSummariesRequest = {
    run_per_thread: selection["per-thread"],
    run_daily: selection.daily,
    run_weekly: selection.weekly,
    run_monthly: selection.monthly,
    ...EMPTY_TOKENS,
    timezone_offset_hours: tzOffsetHours,
  };

  const hasFrom = from.length > 0;
  const hasTo = to.length > 0;
  if (hasFrom !== hasTo) return { request: null, error: "one-sided" };

  if (!hasFrom) {
    if (top === "per-thread") return { request: base, error: null };
    // Resolve the fallback in the workflow's tz so it matches each batch's own
    // `now_utc + offset` fallback (see fallbackDayRange).
    const fb = fallbackDayRange(top, tzOffsetHours, new Date(), timeZone);
    if (!fb) return { request: null, error: "invalid" };
    return { request: applyDayRange(base, selection, top, fb, timeZone), error: null };
  }

  const span = expandToDayRange(top, from, to);
  if (!span) return { request: null, error: "invalid" };
  // String compare works for all three token formats once expanded to YYYY-MM-DD.
  if (span.fromDate > span.toDate) return { request: null, error: "reversed" };
  return { request: applyDayRange(base, selection, top, span, timeZone), error: null };
}

/** Derive every layer's input from the selected `periodSpan`.
 *
 *  weekly/monthly runs extend the daily / per-thread START back to its ISO
 *  week's Monday so the leading boundary week has all its source days. The END
 *  is left at the selected boundary on purpose: extending it into the next
 *  week would push the trailing week's updated_at past the month end and drop
 *  it from this month's monthly summary (see extendStartToWeekStart). daily /
 *  per-thread runs use the span as-is. */
function applyDayRange(
  base: GenerateSummariesRequest,
  selection: KindSelection,
  top: SummaryKind,
  periodSpan: DayRange,
  timeZone?: string,
): GenerateSummariesRequest {
  const dailySpan =
    top === "weekly" || top === "monthly" ? extendStartToWeekStart(periodSpan) : periodSpan;
  const req: GenerateSummariesRequest = {
    ...base,
    ...dayRangeToPeriodTokens(periodSpan, dailySpan),
  };
  if (selection["per-thread"]) {
    const { after, before } = dayRangeToEpochMs(dailySpan.fromDate, dailySpan.toDate, timeZone);
    if (after !== undefined) req.updated_after_ms = after;
    if (before !== undefined) req.updated_before_ms = before;
  }
  return req;
}

function initialSelection(initialKind: SummaryKind): KindSelection {
  const sel: KindSelection = {
    "per-thread": false,
    daily: false,
    weekly: false,
    monthly: false,
  };
  return applyDependency(sel, initialKind, true);
}

export function SummaryGenerateDialog({
  onClose,
  onStarted,
  initialKind,
  sidecar,
}: SummaryGenerateDialogProps) {
  const { t } = useTranslation();
  const [selection, setSelection] = useState<KindSelection>(() => initialSelection(initialKind));
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const llmDown = hasLlmInitFailure(sidecar);

  const top = topKind(selection);
  // Fallback "yesterday / last week / last month" and the per-thread window are
  // resolved in the display timezone so a manual run with no explicit range
  // targets the same day the workflow's own `env.TZ` boundary would.
  const timezone = useTimezone();
  const tzOffsetHours = useMemo(
    () => resolveTimezoneOffsetHours(undefined, 9, timezone),
    [timezone],
  );

  const build = useMemo(
    () => buildGenerateRequest(selection, from, to, tzOffsetHours, timezone),
    [selection, from, to, tzOffsetHours, timezone],
  );

  const toggle = (k: SummaryKind, next: boolean) => {
    setSelection((cur) => applyDependency(cur, k, next));
  };

  const handleSubmit = async () => {
    if (!build.request) return;
    setError(null);
    setBusy(true);
    try {
      // Frontend-generated dispatch id doubles as the cancel key:
      // analysis_cancel(dispatch_id) targets the same in-flight map
      // entry the backend registers from this request, so the Stop
      // button in the toolbar (Summaries.tsx) hits the right job.
      const dispatch_id = crypto.randomUUID();
      const res = await generateSummaries({ ...build.request, dispatch_id });
      onStarted(res.job_id_hint);
      onClose();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal onClose={onClose} ariaLabel={t("summaryGen.title")}>
      <div className="modal-head">
        <div className="modal-title">{t("summaryGen.title")}</div>
      </div>
      <div className="modal-body">
        {llmDown && (
          <div className="warning-banner">
            <div className="warning-banner-title">{t("summaryGen.llmDownTitle")}</div>
            <div className="warning-banner-body">{t("summaryGen.llmDownBody")}</div>
          </div>
        )}

        <div className="field">
          <span className="field-label">{t("summaryGen.stages")}</span>
          {KIND_ORDER.map((k) => (
            <label key={k} className="checkbox-row">
              <input
                type="checkbox"
                checked={selection[k]}
                onChange={(e) => toggle(k, e.target.checked)}
              />
              {t(KIND_LABEL_KEYS[k])}
            </label>
          ))}
          <div className="field-hint">{t("summaryGen.dependencyHint")}</div>
        </div>

        {top != null && (
          <div className="field">
            <span className="field-label">{t("summaryGen.targetPeriod")}</span>
            <div className="radio-row">
              <DateInput
                type={PICKER_TYPE[top]}
                value={from}
                onChange={setFrom}
                title={t("summaryGen.rangeStart")}
              />
              <span style={{ fontSize: 12 }}>〜</span>
              <DateInput
                type={PICKER_TYPE[top]}
                value={to}
                onChange={setTo}
                title={t("summaryGen.rangeEnd")}
              />
            </div>
            <div className="field-hint">{t(RANGE_HINT_KEY[top])}</div>
            {build.error && <div className="form-error">{t(BUILD_ERROR_KEY[build.error])}</div>}
          </div>
        )}

        {error && <div className="form-error">{error}</div>}
      </div>
      <div className="modal-foot">
        <button type="button" className="btn" onClick={onClose} disabled={busy}>
          {t("common.cancel")}
        </button>
        <button
          type="button"
          className="btn primary"
          onClick={() => void handleSubmit()}
          disabled={busy || llmDown || build.request == null}
        >
          {busy ? t("summaryGen.submitting") : t("summaryGen.submit")}
        </button>
      </div>
    </Modal>
  );
}
