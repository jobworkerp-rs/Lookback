import { describe, expect, it } from "vitest";
import dailyBatch from "../workers/workflows/daily-work-summary/daily-work-summary-batch.yaml?raw";
import periodicRun from "../workers/workflows/lookback-periodic-run.yaml?raw";
import monthlyBatch from "../workers/workflows/monthly-work-summary/monthly-work-summary-batch.yaml?raw";
import personalityBatch from "../workers/workflows/personality/thread-personality-batch.yaml?raw";
import groupBatch from "../workers/workflows/thread-group-summary/thread-group-summary-batch.yaml?raw";
import reflectionBatch from "../workers/workflows/thread-reflection/thread-reflection-batch.yaml?raw";
import threadBatch from "../workers/workflows/thread-summary/thread-summary-batch.yaml?raw";
import weeklyBatch from "../workers/workflows/weekly-work-summary/weekly-work-summary-batch.yaml?raw";
import { runStep } from "./lib/workflowJq";

const UNACCOUNTED = "ended without an outcome";

// Runs a batch's collect + record pair the way the engine would and returns
// the state the batch output reads afterwards.
function account(
  workflow: string,
  unit: string,
  failedVar: string,
  unaccountedVar: string,
  bindings: Record<string, unknown>,
) {
  const unaccounted = runStep(workflow, `collectUnaccounted${unit}`, unaccountedVar, bindings);
  const state = { ...bindings, [unaccountedVar]: unaccounted };
  if (!runStep(workflow, `recordUnaccounted${unit}`, "if", state)) {
    return { failed: bindings[failedVar], lastError: bindings.last_error };
  }
  return {
    failed: runStep(workflow, `recordUnaccounted${unit}`, failedVar, state),
    lastError: runStep(workflow, `recordUnaccounted${unit}`, "last_error", state),
  };
}

describe("batch outcome accounting", () => {
  const threads = [{ id: { value: 1 } }, { id: { value: 2 } }, { id: { value: 3 } }];
  const threadState = (generated: string[], skipped: string[], failed: string[], lastError: string | null) => ({
    target_threads: threads,
    generated_threads: generated,
    skipped_threads: skipped,
    failed_threads: failed,
    last_error: lastError,
  });
  const accountThreads = (state: Record<string, unknown>) =>
    account(threadBatch, "Threads", "failed_threads", "unaccounted_threads", state);

  it("counts a thread whose item timed out as failed", () => {
    const result = accountThreads(threadState(["1"], [], ["2"], null));
    expect(result.failed).toEqual(["2", "3"]);
    expect(result.lastError).toContain(UNACCOUNTED);
  });

  it("leaves a fully accounted thread batch untouched", () => {
    expect(accountThreads(threadState(["1"], ["2"], ["3"], "llm error"))).toEqual({
      failed: ["3"],
      lastError: "llm error",
    });
  });

  it("keeps the first real error when items are also unaccounted", () => {
    expect(accountThreads(threadState([], [], ["1"], "llm error"))).toEqual({
      failed: ["1", "2", "3"],
      lastError: "llm error",
    });
  });

  const periodCases: [string, string, string, string[]][] = [
    [dailyBatch, "Dates", "date_list", ["2026-10-05", "2026-10-06"]],
    [weeklyBatch, "Weeks", "week_list", ["2026-W40", "2026-W41"]],
    [monthlyBatch, "Months", "month_list", ["2026-09", "2026-10"]],
  ];
  it.each(periodCases)("counts an unaccounted period as failed (%#)", (workflow, unit, listVar, periods) => {
    const u = unit.toLowerCase();
    const result = account(workflow, unit, `failed_${u}`, `unaccounted_${u}`, {
      [listVar]: periods,
      [`generated_${u}`]: [periods[0]],
      [`skipped_${u}`]: [],
      [`failed_${u}`]: [],
      last_error: null,
    });
    expect(result.failed).toEqual([periods[1]]);
    expect(result.lastError).toContain(UNACCOUNTED);
  });

  it("records an unaccounted ThreadGroup with a retryable failure entry", () => {
    const result = account(groupBatch, "Groups", "failed_groups", "unaccounted_groups", {
      target_groups: [
        { id: { value: 10 }, membershipSnapshotDigest: "d10" },
        { id: { value: 11 }, membershipSnapshotDigest: "d11" },
        { id: { value: 12 }, membershipSnapshotDigest: "d12" },
      ],
      generated_groups: ["10"],
      skipped_groups: [],
      stale_groups: [],
      failed_groups: [{ group_id: "11", reason_code: "workflow_failed" }],
      last_error: null,
      retry_digest_by_group: { "10": "d10", "11": "d11", "12": "d12" },
    });
    expect(result.failed).toEqual([
      { group_id: "11", reason_code: "workflow_failed" },
      {
        group_id: "12",
        expected_membership_snapshot_digest: "d12",
        reason_code: "workflow_failed",
        detail: `group ${UNACCOUNTED} (timed out or aborted)`,
      },
    ]);
    expect(result.lastError).toContain(UNACCOUNTED);
  });

  const countCases: [string, Record<string, number>, number][] = [
    [personalityBatch, { budget_skipped_count: 1 }, 1],
    [reflectionBatch, {}, 2],
  ];
  it.each(countCases)("adds unaccounted threads to the failure count (%#)", (workflow, extra, unaccounted) => {
    const result = account(workflow, "Threads", "failed_count", "unaccounted_count", {
      target_threads: [{}, {}, {}, {}],
      succeeded_count: 1,
      failed_count: 1,
      last_error: null,
      ...extra,
    });
    expect(result.failed).toBe(1 + unaccounted);
    expect(result.lastError).toContain(UNACCOUNTED);
  });

  it("does not change a fully accounted personality batch", () => {
    const result = account(personalityBatch, "Threads", "failed_count", "unaccounted_count", {
      target_threads: [{}, {}],
      succeeded_count: 1,
      failed_count: 0,
      budget_skipped_count: 1,
      last_error: null,
    });
    expect(result).toEqual({ failed: 0, lastError: null });
  });
});

describe("periodic stage outcome aggregation", () => {
  const periodicState = (overrides: Record<string, unknown>) => ({
    thread_result: { completed: true, failed_count: 0 },
    thread_group_result: null,
    daily_result: null,
    weekly_result: null,
    monthly_result: null,
    personality_result: null,
    reflection_result: null,
    ...overrides,
  });
  const incompletePeriodicStages = (state: Record<string, unknown>) =>
    runStep(periodicRun, "collectIncompletePeriodicStages", "incomplete_periodic_stages", state);

  it("fails a periodic run with each incomplete stage and its last error", () => {
    const stages = incompletePeriodicStages(
      periodicState({
        thread_group_result: { completed: false, failed_count: 0 },
        reflection_result: { completed: true, failed_count: "2", last_error: "llm 429" },
      }),
    );
    expect(stages).toEqual([
      { stage: "thread_group", last_error: null },
      { stage: "reflection", last_error: "llm 429" },
    ]);
    const state = { incomplete_periodic_stages: stages };
    expect(runStep(periodicRun, "failIfAnyStageIncomplete", "if", state)).toBe(true);
    expect(runStep(periodicRun, "failIfAnyStageIncomplete", "detail", state)).toBe(
      "thread_group: incomplete; reflection: llm 429",
    );
  });

  it("lets a clean periodic run succeed", () => {
    const stages = incompletePeriodicStages(periodicState({}));
    expect(stages).toEqual([]);
    expect(runStep(periodicRun, "failIfAnyStageIncomplete", "if", { incomplete_periodic_stages: stages })).toBe(
      false,
    );
  });
});
