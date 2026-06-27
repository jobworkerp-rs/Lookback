import { beforeAll, describe, expect, it } from "vitest";
import i18n from "@/i18n";
import type { PeriodicExecutionStatus } from "@/types/api";
import {
  cronPreview,
  DEFAULT_PERIODIC_DRAFT,
  draftFromTask,
  formatExecutionTime,
  PERIODIC_COMBINED_SOURCE,
  periodicKindLabel,
  periodicSourceLabel,
  periodicStatusLabel,
  periodicStatusVariant,
  stableSchedulerIds,
  toTaskArgs,
  validatePeriodicDraft,
} from "./periodicTask";

const ALL_STATUSES: PeriodicExecutionStatus[] = [
  "pending",
  "running",
  "wait_result",
  "cancelling",
  "succeeded",
  "failed",
  "cancelled",
  "unknown",
  "unavailable",
  "enqueue_failed",
  "not_started",
];

// Resolve labels through the real dictionary pinned to Japanese, so the
// assertions read the same wording a ja user sees.
const t = i18n.t.bind(i18n);

beforeAll(() => {
  i18n.changeLanguage("ja");
});

describe("periodic status helpers", () => {
  it("maps every status to a non-empty Japanese label", () => {
    for (const status of ALL_STATUSES) {
      expect(periodicStatusLabel(t, status)).toBeTruthy();
    }
    expect(periodicStatusLabel(t, "running")).toBe("実行中");
    expect(periodicStatusLabel(t, "not_started")).toBe("未実行");
    expect(periodicStatusLabel(t, "cancelling")).toBe("取消中");
  });

  it("maps every status to a known badge variant", () => {
    const variants = new Set(["active", "success", "error", "neutral", "warning"]);
    for (const status of ALL_STATUSES) {
      expect(variants.has(periodicStatusVariant(status))).toBe(true);
    }
    expect(periodicStatusVariant("running")).toBe("active");
    expect(periodicStatusVariant("succeeded")).toBe("success");
    expect(periodicStatusVariant("failed")).toBe("error");
  });

  it("stableSchedulerIds dedups and sorts ascending", () => {
    expect(stableSchedulerIds(["3", "1", "3", "2", "1"])).toEqual(["1", "2", "3"]);
    expect(stableSchedulerIds([])).toEqual([]);
  });

  it("formatExecutionTime returns null for null / non-positive ms", () => {
    expect(formatExecutionTime(null)).toBeNull();
    expect(formatExecutionTime(0)).toBeNull();
    expect(formatExecutionTime(-1)).toBeNull();
    expect(formatExecutionTime(1_700_000_000_000)).toContain("20");
  });
});

describe("periodicTask form logic", () => {
  it("builds a six-field cron preview from the baseline time for regular hourly tasks", () => {
    const task = toTaskArgs({
      ...DEFAULT_PERIODIC_DRAFT,
      name: "朝",
      interval_mode: "hours",
      interval_value: 6,
      hour: 9,
      minute: 30,
      run_summary_thread: true,
    });
    expect(cronPreview(t, task)).toBe("0 30 3,9,15,21 * * *");
    expect(task.force_thread_summary).toBe(true);
  });

  it("builds hourly cron previews for intervals that cross midnight", () => {
    expect(
      cronPreview(
        t,
        toTaskArgs({
          ...DEFAULT_PERIODIC_DRAFT,
          name: "8時間",
          interval_mode: "hours",
          interval_value: 8,
          hour: 9,
          minute: 30,
        }),
      ),
    ).toBe("0 30 1,9,17 * * *");

    expect(
      cronPreview(
        t,
        toTaskArgs({
          ...DEFAULT_PERIODIC_DRAFT,
          name: "24時間",
          interval_mode: "hours",
          interval_value: 24,
          hour: 9,
          minute: 30,
        }),
      ),
    ).toBe("0 30 9 * * *");
  });

  it("builds day-based cron previews with the selected time", () => {
    const task = toTaskArgs({
      ...DEFAULT_PERIODIC_DRAFT,
      name: "3日おき",
      interval_mode: "days",
      interval_value: 3,
      hour: 9,
      minute: 30,
    });

    expect(cronPreview(t, task)).toBe("0 30 9 */3 * *");
  });

  it("defaults a name-only regular task to the basic summary stages, not a no-op", () => {
    // Regression guard: with all generation flags off the periodic workflow
    // fires but skips every flag-gated stage, so the default draft must enable
    // at least the thread + daily summary stages.
    const def = toTaskArgs({
      ...DEFAULT_PERIODIC_DRAFT,
      name: "朝の要約",
    });
    expect(def.force_thread_summary).toBe(true);
    expect(def.run_summary_daily).toBe(true);
    expect(def.run_personality).toBe(false);
    expect(def.run_reflection).toBe(false);
  });

  it("serializes an explicit opt-out of every generation stage", () => {
    const noneSelected = toTaskArgs({
      ...DEFAULT_PERIODIC_DRAFT,
      name: "生成なし",
      run_summary_thread: false,
      run_summary_daily: false,
      run_personality: false,
      run_reflection: false,
    });
    expect(noneSelected.force_thread_summary).toBe(false);
    expect(noneSelected.run_summary_daily).toBe(false);
    expect(noneSelected.run_personality).toBe(false);
    expect(noneSelected.run_reflection).toBe(false);
  });

  it("serializes generation flags and forces thread summary when daily is selected", () => {
    const daily = toTaskArgs({
      ...DEFAULT_PERIODIC_DRAFT,
      name: "日次",
      run_summary_thread: false,
      run_summary_daily: true,
      run_personality: true,
      run_reflection: true,
    });
    expect(daily.force_thread_summary).toBe(true);
    expect(daily.run_summary_daily).toBe(true);
    expect(daily.run_personality).toBe(true);
    expect(daily.run_reflection).toBe(true);
    expect(draftFromTask(daily).run_summary_thread).toBe(true);
  });

  it("restores legacy regular tasks without enabling newly added generators", () => {
    const legacy = draftFromTask({
      name: "旧定期",
      source: "codex",
      sources: ["codex"],
      task_kind: "regular",
      hour: 9,
      minute: 0,
      interval_hours: 24,
      interval_days: null,
      weekly_day: null,
      monthly_day: null,
      lookback_days: 7,
      force_thread_summary: true,
    });

    expect(legacy.run_summary_thread).toBe(true);
    expect(legacy.run_summary_daily).toBe(true);
    expect(legacy.run_personality).toBe(false);
    expect(legacy.run_reflection).toBe(false);
  });

  it("builds weekly and monthly labels/previews", () => {
    const weekly = toTaskArgs({
      ...DEFAULT_PERIODIC_DRAFT,
      name: "週次",
      task_kind: "weekly",
      weekly_day: 1,
      minute: 0,
    });
    expect(cronPreview(t, weekly)).toBe("0 0 9 * * 1");
    expect(periodicKindLabel(t, "weekly")).toBe("週次");

    const monthly = toTaskArgs({
      ...DEFAULT_PERIODIC_DRAFT,
      name: "月次",
      task_kind: "monthly",
      monthly_day: 28,
    });
    expect(cronPreview(t, monthly)).toBe("0 0 9 28 * *");
    expect(periodicKindLabel(t, "monthly")).toBe("月次");
  });

  it("serializes the combined codex and claude-code source mode", () => {
    const task = toTaskArgs({
      ...DEFAULT_PERIODIC_DRAFT,
      name: "まとめて要約",
      source: PERIODIC_COMBINED_SOURCE,
    });

    expect(task.source).toBe(PERIODIC_COMBINED_SOURCE);
    expect(task.sources).toEqual(["codex", "claude-code"]);
    expect(periodicSourceLabel(t, task.source, task.sources)).toBe("codex + claude-code");
    expect(draftFromTask(task).source).toBe(PERIODIC_COMBINED_SOURCE);
  });

  it("validates missing source, invalid lookback, and monthly boundary", () => {
    expect(
      validatePeriodicDraft({
        ...DEFAULT_PERIODIC_DRAFT,
        name: "",
        source: "",
        lookback_days: 0,
      }),
    ).toEqual([
      "periodic.validation.nameRequired",
      "periodic.validation.sourceRequired",
      "periodic.validation.lookbackRange",
    ]);

    expect(
      validatePeriodicDraft({
        ...DEFAULT_PERIODIC_DRAFT,
        name: "月次",
        task_kind: "monthly",
        monthly_day: 29,
      }),
    ).toContain("periodic.validation.monthlyDayRange");

    expect(
      validatePeriodicDraft({
        ...DEFAULT_PERIODIC_DRAFT,
        name: "plain",
        source: "plain",
      }),
    ).toContain("periodic.validation.sourceInvalid");
  });

  it("rejects unsupported regular hour intervals", () => {
    expect(
      validatePeriodicDraft({
        ...DEFAULT_PERIODIC_DRAFT,
        name: "5時間",
        interval_mode: "hours",
        interval_value: 5,
      }),
    ).toContain("periodic.validation.intervalHourChoice");
  });
});
