import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { generateSummaries } from "@/api";
import { MondayDatePicker } from "@/components/MondayDatePicker";
import { MonthPicker } from "@/components/MonthPicker";
import { hasLlmInitFailure, type SidecarStatus } from "@/hooks/useSidecarStatus";
import { useTimezone } from "@/hooks/useTimezone";
import { dayRangeToEpochMs, resolveTimezoneOffsetHours } from "@/lib/dateInput";
import {
  applyDependency,
  KIND_LABEL_KEYS,
  type KindSelection,
  PUBLIC_PERIOD_KIND_ORDER,
  topKind,
} from "@/lib/summaryKind";
import {
  compareDayKeys,
  type DayRange,
  dayRangeToPeriodTokens,
  expandDateRangeToContainingWeeks,
  expandToDayRange,
  extendStartToWeekStart,
  fallbackDayRange,
  type PeriodTokens,
} from "@/lib/summaryPeriod";
import type { GenerateSummariesRequest, SummaryKind } from "@/types/api";
import { Modal } from "./Modal";

export interface SummaryGenerateDialogProps {
  onClose: () => void;
  /** Resets the progress slot before dispatch so the first event cannot race it. */
  onStart: (dispatchId: string) => void;
  /** Releases the pre-dispatch slot if invoke rejects before a run is created. */
  onDispatchError: (dispatchId: string) => void;
  /** Granularity to preselect — the page mounts this dialog fresh each open,
   *  so it always reflects the page's current tab. */
  initialKind: Exclude<SummaryKind, "per-thread">;
  sidecar: SidecarStatus;
}

/** Native picker type per top-most granularity. Weekly ranges use dates so
 *  selecting a period does not depend on OS support for `input[type=week]`. */
const PICKER_TYPE: Record<Exclude<SummaryKind, "thread-group">, "date" | "month"> = {
  "per-thread": "date",
  daily: "date",
  weekly: "date",
  monthly: "month",
};

/** i18n key per granularity for the range hint, resolved with `t` at render. */
const RANGE_HINT_KEY: Record<Exclude<SummaryKind, "thread-group">, string> = {
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

/** User-facing manual stages. Per-thread is an implicit dependency and is
 * intentionally omitted. */
const MANUAL_STAGE_KIND_ORDER: readonly Exclude<SummaryKind, "per-thread">[] = [
  "thread-group",
  ...PUBLIC_PERIOD_KIND_ORDER,
];

/** i18n key per build error, resolved with `t` at render. */
const BUILD_ERROR_KEY: Record<BuildError, string> = {
  "one-sided": "summaryGen.errorOneSided",
  reversed: "summaryGen.errorReversed",
  invalid: "summaryGen.errorInvalid",
  required: "summaryGen.errorRequired",
};

/** Why a build failed — lets the dialog show a specific message instead of
 *  re-deriving the reason from a null result. `null` request + `null` reason =
 *  nothing selected (no message needed). */
export type BuildError = "one-sided" | "reversed" | "invalid" | "required";

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
  if (!Object.values(selection).some(Boolean)) return { request: null, error: null };

  const base: GenerateSummariesRequest = {
    run_per_thread: selection["per-thread"],
    run_thread_group: selection["thread-group"],
    run_daily: selection.daily,
    run_weekly: selection.weekly,
    run_monthly: selection.monthly,
    stop_on_error: false,
    ...EMPTY_TOKENS,
    timezone_offset_hours: tzOffsetHours,
  };

  if (!top) return { request: base, error: null };

  const hasFrom = from.length > 0;
  const hasTo = to.length > 0;
  if (hasFrom !== hasTo) return { request: null, error: "one-sided" };

  if (!hasFrom) {
    // ThreadGroups are selected by member activity in the period, so a
    // ThreadGroup run without one would visit every group.
    if (top === "per-thread") {
      return selection["thread-group"]
        ? { request: null, error: "required" }
        : { request: base, error: null };
    }
    // Resolve the fallback in the workflow's tz so it matches each batch's own
    // `now_utc + offset` fallback (see fallbackDayRange).
    const fb = fallbackDayRange(top, tzOffsetHours, new Date(), timeZone);
    if (!fb) return { request: null, error: "invalid" };
    return { request: applyDayRange(base, selection, top, fb, timeZone), error: null };
  }

  // Weekly date selection is expanded to complete weeks below. Check the
  // user's dates first, otherwise a reverse order inside one week collapses
  // into the same Monday..Sunday range and incorrectly becomes valid.
  if (top === "weekly" && (compareDayKeys(from, to) ?? 0) > 0) {
    return { request: null, error: "reversed" };
  }

  const span =
    top === "weekly" ? expandDateRangeToContainingWeeks(from, to) : expandToDayRange(top, from, to);
  if (!span) return { request: null, error: "invalid" };
  // String compare works for all three token formats once expanded to YYYY-MM-DD.
  if (span.fromDate > span.toDate) return { request: null, error: "reversed" };
  return { request: applyDayRange(base, selection, top, span, timeZone), error: null };
}

/** The period a request covers at its top granularity (`a〜b`, or one key
 *  when both ends match), or null when it is unbounded. Exported for tests. */
export function targetRange(
  top: Exclude<SummaryKind, "thread-group">,
  request: GenerateSummariesRequest | null,
  from: string,
  to: string,
): string | null {
  if (!request) return null;
  const [start, end] =
    top === "monthly"
      ? [request.monthly_start, request.monthly_end]
      : top === "weekly"
        ? [request.weekly_start, request.weekly_end]
        : top === "daily"
          ? [request.daily_start, request.daily_end]
          : [from, to];
  if (!start || !end) return null;
  return start === end ? start : `${start}〜${end}`;
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
  const { after, before } = dayRangeToEpochMs(dailySpan.fromDate, dailySpan.toDate, timeZone);
  if (selection["per-thread"]) {
    if (after !== undefined) req.last_message_after_ms = after;
    if (before !== undefined) req.last_message_before_ms = before;
  }
  // SearchThreadGroups treats both bounds as exclusive, while `before` is the
  // inclusive last millisecond of the span.
  if (selection["thread-group"] && after !== undefined && before !== undefined) {
    req.thread_group_activity_after_ms = after;
    req.thread_group_activity_before_ms = before + 1;
  }
  return req;
}

function initialSelection(initialKind: Exclude<SummaryKind, "per-thread">): KindSelection {
  const sel: KindSelection = {
    "per-thread": false,
    "thread-group": false,
    daily: false,
    weekly: false,
    monthly: false,
  };
  return applyDependency(sel, initialKind, true);
}

export function SummaryGenerateDialog({
  onClose,
  onStart,
  onDispatchError,
  initialKind,
  sidecar,
}: SummaryGenerateDialogProps) {
  const { t } = useTranslation();
  const [selection, setSelection] = useState<KindSelection>(() => initialSelection(initialKind));
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  const [stopOnError, setStopOnError] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const llmDown = hasLlmInitFailure(sidecar);

  const hasSelectedStage = MANUAL_STAGE_KIND_ORDER.some((kind) => selection[kind]);
  // Per-thread alone is an implicit leftover, not a user-selected stage.
  const top = hasSelectedStage ? topKind(selection) : null;
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

  // The period actually sent, so an unset range shows its fallback too.
  const range = top == null ? null : targetRange(top, build.request, from, to);

  const toggle = (k: Exclude<SummaryKind, "per-thread">, next: boolean) => {
    setSelection((cur) => applyDependency(cur, k, next));
  };

  const handleSubmit = async () => {
    if (!hasSelectedStage || !build.request) return;
    setError(null);
    setBusy(true);
    let dispatchId: string | undefined;
    try {
      // Frontend-generated dispatch id doubles as the cancel key:
      // analysis_cancel(dispatch_id) targets the same in-flight map
      // entry the backend registers from this request, so the Stop
      // button in the toolbar (Summaries.tsx) hits the right job.
      dispatchId = crypto.randomUUID();
      onStart(dispatchId);
      await generateSummaries({
        ...build.request,
        stop_on_error: stopOnError,
        dispatch_id: dispatchId,
      });
      onClose();
    } catch (e) {
      if (dispatchId) onDispatchError(dispatchId);
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
          {MANUAL_STAGE_KIND_ORDER.map((k) => (
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

        <div className="field">
          <label className="checkbox-row">
            <input
              type="checkbox"
              checked={stopOnError}
              onChange={(e) => setStopOnError(e.target.checked)}
            />
            {t("summaryGen.stopOnError")}
          </label>
        </div>

        {top != null && (
          <div className="field">
            <span className="field-label">{t("summaryGen.targetPeriod")}</span>
            <div className="radio-row">
              {PICKER_TYPE[top] === "date" ? (
                <MondayDatePicker
                  value={from}
                  onChange={setFrom}
                  title={t("summaryGen.rangeStart")}
                />
              ) : (
                <MonthPicker value={from} onChange={setFrom} title={t("summaryGen.rangeStart")} />
              )}
              <span style={{ fontSize: 12 }}>〜</span>
              {PICKER_TYPE[top] === "date" ? (
                <MondayDatePicker value={to} onChange={setTo} title={t("summaryGen.rangeEnd")} />
              ) : (
                <MonthPicker value={to} onChange={setTo} title={t("summaryGen.rangeEnd")} />
              )}
            </div>
            <div className="field-hint">{t(RANGE_HINT_KEY[top])}</div>
            {range && <div className="field-hint">{t("summaryGen.targetRange", { range })}</div>}
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
          disabled={busy || llmDown || !hasSelectedStage || build.request == null}
        >
          {busy ? t("summaryGen.submitting") : t("summaryGen.submit")}
        </button>
      </div>
    </Modal>
  );
}
