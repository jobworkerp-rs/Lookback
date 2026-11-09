import type { TFunction } from "i18next";
import type { PeriodicExecutionStatus, PeriodicTaskArgs, PeriodicTaskKind } from "@/types/api";
import { formatDateTime } from "./localeFormat";

export const PERIODIC_SOURCE_CHOICES = ["codex", "claude-code", "opencode"] as const;
export type PeriodicSource = (typeof PERIODIC_SOURCE_CHOICES)[number];
export const PERIODIC_INTERVAL_HOUR_OPTIONS = [1, 2, 3, 4, 6, 8, 12, 24] as const;

const PERIODIC_INTERVAL_HOUR_VALUES = new Set<number>(PERIODIC_INTERVAL_HOUR_OPTIONS);

const PERIODIC_SOURCE_VALUES = new Set<string>(PERIODIC_SOURCE_CHOICES);

// Vocabulary display order used to canonicalize any selection; the Rust side
// (effective_sources) sorts with the same ranking, so both layers agree.
const PERIODIC_SOURCE_ORDER = new Map<string, number>(
  PERIODIC_SOURCE_CHOICES.map((source, index) => [source, index]),
);

export interface PeriodicTaskDraft {
  name: string;
  sources: string[];
  task_kind: PeriodicTaskKind;
  hour: number;
  minute: number;
  interval_mode: "hours" | "days";
  interval_value: number;
  weekly_day: number;
  monthly_day: number;
  lookback_days: number;
  run_summary_thread: boolean;
  run_summary_thread_group: boolean;
  run_summary_daily: boolean;
  run_personality: boolean;
  run_reflection: boolean;
  stop_on_error: boolean;
}

export const DEFAULT_PERIODIC_DRAFT: PeriodicTaskDraft = {
  name: "",
  // Default to every supported source (codex + claude-code + opencode) so a
  // name-only save imports all agent logs, matching the seeded combined tasks.
  sources: [...PERIODIC_SOURCE_CHOICES],
  task_kind: "regular",
  hour: 9,
  minute: 0,
  interval_mode: "hours",
  interval_value: 24,
  weekly_day: 1,
  monthly_day: 1,
  lookback_days: 7,
  // Default a regular task to the two lightweight summary stages so a
  // name-only save actually produces output; without one of these flags the
  // periodic workflow fires but skips every (flag-gated) generation stage.
  // Personality/reflection stay off — they are the heavy passes a user opts
  // into. Mirrors the YAML schema defaults (force_thread_summary /
  // run_summary_daily both default true) and the Rust serde defaults.
  run_summary_thread: true,
  run_summary_thread_group: true,
  run_summary_daily: true,
  run_personality: false,
  run_reflection: false,
  stop_on_error: false,
};

// Canonicalize any source list: keep known vocabulary items, dedupe, and sort
// into the shared display order. Unknown entries are dropped because the
// backend rejects them before persistence anyway.
export function canonicalPeriodicSources(sources?: string[] | null): string[] {
  return [...new Set(sources ?? [])]
    .filter((source) => PERIODIC_SOURCE_VALUES.has(source))
    .sort((a, b) => (PERIODIC_SOURCE_ORDER.get(a) ?? 99) - (PERIODIC_SOURCE_ORDER.get(b) ?? 99));
}

// Legacy persisted rows may omit `sources` and encode the selection in the
// singular `source` field with "+" separators ("codex+claude-code").
export function periodicSourceLabels(source?: string | null, sources?: string[] | null): string[] {
  const canonical = canonicalPeriodicSources(sources);
  if (canonical.length > 0) return canonical;
  return (source ?? "")
    .split("+")
    .map((part) => part.trim())
    .filter(Boolean);
}

export function periodicSourceLabel(
  t: TFunction,
  source: string,
  sources?: string[] | null,
): string {
  const labels = periodicSourceLabels(source, sources);
  if (labels.length === 0) return t("periodic.unknownSource");
  return labels.join(" + ");
}

export function toTaskArgs(draft: PeriodicTaskDraft): PeriodicTaskArgs {
  const sources = canonicalPeriodicSources(draft.sources);
  const regular = draft.task_kind === "regular";
  const runSummaryDaily = regular && draft.run_summary_daily;
  const runSummaryThread = draft.run_summary_thread || runSummaryDaily;
  const runSummaryThreadGroup = draft.run_summary_thread_group;
  return {
    name: draft.name.trim(),
    // `source` is a display/legacy column: singular value when one source is
    // selected, otherwise the "+"-joined selection (matches the backend
    // validator and the legacy "codex+claude-code" combined token).
    source: sources.join("+"),
    sources,
    task_kind: draft.task_kind,
    hour: draft.hour,
    minute: draft.minute,
    interval_hours:
      draft.task_kind === "regular" && draft.interval_mode === "hours"
        ? draft.interval_value
        : null,
    interval_days:
      draft.task_kind === "regular" && draft.interval_mode === "days" ? draft.interval_value : null,
    weekly_day: draft.task_kind === "weekly" ? draft.weekly_day : null,
    monthly_day: draft.task_kind === "monthly" ? draft.monthly_day : null,
    lookback_days: draft.lookback_days,
    // ThreadGroup aggregation consumes thread summaries, so opting into that
    // stage always keeps its dependency enabled.
    force_thread_summary: runSummaryThread || runSummaryThreadGroup,
    run_summary_thread_group: runSummaryThreadGroup,
    run_summary_daily: runSummaryDaily,
    run_personality: regular && draft.run_personality,
    run_reflection: regular && draft.run_reflection,
    stop_on_error: draft.stop_on_error,
  };
}

export function draftFromTask(task: PeriodicTaskArgs): PeriodicTaskDraft {
  return {
    ...DEFAULT_PERIODIC_DRAFT,
    name: task.name,
    sources: periodicSourceLabels(task.source, task.sources),
    task_kind: task.task_kind,
    hour: task.hour,
    minute: task.minute,
    interval_mode: task.interval_days ? "days" : "hours",
    interval_value: task.interval_days ?? task.interval_hours ?? 24,
    weekly_day: task.weekly_day ?? 1,
    monthly_day: task.monthly_day ?? 1,
    lookback_days: task.lookback_days,
    run_summary_thread: task.force_thread_summary,
    run_summary_thread_group:
      task.run_summary_thread_group ?? (task.task_kind === "regular" && task.force_thread_summary),
    stop_on_error: task.stop_on_error ?? false,
    run_summary_daily: task.task_kind === "regular" ? (task.run_summary_daily ?? true) : true,
    run_personality: task.task_kind === "regular" ? (task.run_personality ?? false) : true,
    run_reflection: task.task_kind === "regular" ? (task.run_reflection ?? false) : true,
  };
}

/**
 * Validate a draft, returning i18n keys (under `periodic.validation.*`) for each
 * failed rule. The caller resolves them via `t()`, keeping this module React-free.
 */
export function validatePeriodicDraft(draft: PeriodicTaskDraft): string[] {
  const errors: string[] = [];
  if (!draft.name.trim()) errors.push("periodic.validation.nameRequired");
  if (draft.sources.length === 0) errors.push("periodic.validation.sourceRequired");
  if (
    draft.sources.length > 0 &&
    draft.sources.some((source) => !PERIODIC_SOURCE_VALUES.has(source))
  ) {
    errors.push("periodic.validation.sourceInvalid");
  }
  if (draft.hour < 0 || draft.hour > 23) errors.push("periodic.validation.hourRange");
  if (draft.minute < 0 || draft.minute > 59) errors.push("periodic.validation.minuteRange");
  if (draft.lookback_days < 0) errors.push("periodic.validation.lookbackRange");
  if (draft.task_kind === "regular" && draft.interval_value <= 0) {
    errors.push("periodic.validation.intervalRange");
  }
  if (
    draft.task_kind === "regular" &&
    draft.interval_mode === "hours" &&
    !PERIODIC_INTERVAL_HOUR_VALUES.has(draft.interval_value)
  ) {
    errors.push("periodic.validation.intervalHourChoice");
  }
  if (draft.task_kind === "weekly" && (draft.weekly_day < 0 || draft.weekly_day > 6)) {
    errors.push("periodic.validation.weekdayInvalid");
  }
  if (draft.task_kind === "monthly" && (draft.monthly_day < 1 || draft.monthly_day > 28)) {
    errors.push("periodic.validation.monthlyDayRange");
  }
  return errors;
}

export function cronPreview(t: TFunction, task: PeriodicTaskArgs): string {
  if (task.task_kind === "regular") {
    if (task.interval_hours != null) {
      const hours = hourListForInterval(task.hour, task.interval_hours);
      if (hours.length === 0) return t("periodic.cronInvalidInterval");
      return `0 ${task.minute} ${hours.join(",")} * * *`;
    }
    return `0 ${task.minute} ${task.hour} */${task.interval_days ?? 1} * *`;
  }
  if (task.task_kind === "weekly") {
    return `0 ${task.minute} ${task.hour} * * ${task.weekly_day ?? 1}`;
  }
  return `0 ${task.minute} ${task.hour} ${task.monthly_day ?? 1} * *`;
}

function hourListForInterval(startHour: number, intervalHours: number): number[] {
  if (!PERIODIC_INTERVAL_HOUR_VALUES.has(intervalHours)) return [];
  const count = 24 / intervalHours;
  return Array.from({ length: count }, (_, index) => (startHour + index * intervalHours) % 24).sort(
    (a, b) => a - b,
  );
}

export function periodicKindLabel(t: TFunction, kind: PeriodicTaskKind): string {
  return t(`periodic.kindLabel.${kind}`);
}

/** Variant suffix for the `.periodic-status-*` badge class (groups statuses by
 *  visual treatment, not 1:1 with the status). */
export type PeriodicStatusVariant = "active" | "success" | "error" | "neutral" | "warning";

// Badge variant per status (label lives in the i18n dictionary under
// `periodic.statusLabel.*`). Adding a status is one row here plus one dict key.
const STATUS_VARIANT: Record<PeriodicExecutionStatus, PeriodicStatusVariant> = {
  pending: "active",
  running: "active",
  wait_result: "active",
  cancelling: "active",
  succeeded: "success",
  failed: "error",
  cancelled: "neutral",
  unknown: "warning",
  unavailable: "warning",
  enqueue_failed: "error",
  not_started: "neutral",
};

export function periodicStatusLabel(t: TFunction, status: PeriodicExecutionStatus): string {
  return t(`periodic.statusLabel.${status}`);
}

export function periodicStatusVariant(status: PeriodicExecutionStatus): PeriodicStatusVariant {
  return STATUS_VARIANT[status];
}

/** Stable, deduped, ascending-sorted scheduler ids for the status query key, so
 *  a reorder of the list doesn't churn the cache. Card display order still uses
 *  the `["periodic-tasks"]` result order. */
export function stableSchedulerIds(ids: string[]): string[] {
  return [...new Set(ids)].sort();
}

/** Epoch ms → timezone display string. `null` / non-positive → null. `timeZone`
 *  (an IANA name) renders in that zone; omit to follow the host zone. */
export function formatExecutionTime(
  ms: number | null,
  locale?: string,
  timeZone?: string,
): string | null {
  if (ms === null || ms <= 0) return null;
  return formatDateTime(ms, locale, timeZone);
}
