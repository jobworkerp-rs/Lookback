import { describe, expect, it } from "vitest";
import dailyWorkflow from "../workers/lang-workers/workers/daily-work-summary/daily-work-summary-single.yaml?raw";
import monthlyWorkflow from "../workers/lang-workers/workers/monthly-work-summary/monthly-work-summary-single.yaml?raw";
import threadSummarySingleWorkflow from "../workers/lang-workers/workers/thread-summary/thread-summary-single.yaml?raw";
import weeklyWorkflow from "../workers/lang-workers/workers/weekly-work-summary/weekly-work-summary-single.yaml?raw";
import dailyBatchWorkflow from "../workers/workflows/daily-work-summary/daily-work-summary-batch.yaml?raw";
import periodicWorkflow from "../workers/workflows/lookback-periodic-run.yaml?raw";
import monthlyBatchWorkflow from "../workers/workflows/monthly-work-summary/monthly-work-summary-batch.yaml?raw";
import threadPersonalityBatchWorkflow from "../workers/workflows/personality/thread-personality-batch.yaml?raw";
import ragWorkflow from "../workers/workflows/rag/lookback-recall.yaml?raw";
import threadReflectionBatchWorkflow from "../workers/workflows/thread-reflection/thread-reflection-batch.yaml?raw";
import threadSummaryBatchWorkflow from "../workers/workflows/thread-summary/thread-summary-batch.yaml?raw";
import weeklyBatchWorkflow from "../workers/workflows/weekly-work-summary/weekly-work-summary-batch.yaml?raw";

function yamlExpression(body: string): string {
  return `"${"$"}{ ${body} }"`;
}

function needsThreadSummary(
  thread: { lastMessageAt: string; updatedAt: string },
  existingSummary: { updatedAt: string },
): boolean {
  return BigInt(thread.lastMessageAt) > BigInt(existingSummary.updatedAt);
}

function sortPersonalityThreadsNewestFirst<
  T extends { id: { value: string }; data: { lastMessageAt: string; updatedAt: string } },
>(threads: T[]): T[] {
  return [...threads].sort((left, right) => {
    const timeOrder = BigInt(right.data.lastMessageAt) - BigInt(left.data.lastMessageAt);
    if (timeOrder !== 0n) return timeOrder > 0n ? 1 : -1;
    const idOrder = BigInt(right.id.value) - BigInt(left.id.value);
    return idOrder === 0n ? 0 : idOrder > 0n ? 1 : -1;
  });
}

describe("summary workflow timezone boundaries", () => {
  it("uses conversation-time windows for manual thread batches", () => {
    for (const yaml of [
      threadSummaryBatchWorkflow,
      threadPersonalityBatchWorkflow,
      threadReflectionBatchWorkflow,
    ]) {
      expect(yaml).toContain("last_message_after");
      expect(yaml).toContain("last_message_within_hours");
      expect(yaml).not.toContain("updated_within_hours");
      expect(yaml).not.toContain("resolveUpdatedAfter");
    }
  });

  it("treats a present empty import ID set as zero targets", () => {
    for (const yaml of [
      threadSummaryBatchWorkflow,
      threadPersonalityBatchWorkflow,
      threadReflectionBatchWorkflow,
    ]) {
      expect(yaml).toContain("target_thread_ids");
      expect(yaml).toContain('has("target_thread_ids")');
      expect(yaml).toContain("thread_ids");
    }
  });

  it("skips thread summaries by conversation freshness, not audit updates", () => {
    expect(threadSummarySingleWorkflow).toContain("$thread.data.lastMessageAt");
    expect(threadSummarySingleWorkflow).not.toContain("$thread.data.updatedAt");
    expect(
      needsThreadSummary({ lastMessageAt: "100", updatedAt: "999" }, { updatedAt: "100" }),
    ).toBe(false);
    expect(
      needsThreadSummary({ lastMessageAt: "101", updatedAt: "101" }, { updatedAt: "100" }),
    ).toBe(true);
  });

  it("sorts personality targets by exact last-message time with a stable ID tie-break", () => {
    expect(threadPersonalityBatchWorkflow).toContain(".data.lastMessageAt");
    expect(threadPersonalityBatchWorkflow).not.toContain(
      "sort_by((.data.updatedAt // 0) | tonumber)",
    );
    const sorted = sortPersonalityThreadsNewestFirst([
      {
        id: { value: "7" },
        data: { lastMessageAt: "9007199254740992", updatedAt: "9999999999999999" },
      },
      {
        id: { value: "8" },
        data: { lastMessageAt: "9007199254740993", updatedAt: "1" },
      },
      {
        id: { value: "9" },
        data: { lastMessageAt: "9007199254740993", updatedAt: "0" },
      },
    ]);
    expect(sorted.map((thread) => thread.id.value)).toEqual(["9", "8", "7"]);
  });

  it("computes daily end from the next local midnight instead of a fixed 24h add", () => {
    expect(dailyWorkflow).not.toContain(
      `day_end_ms: ${yamlExpression("$day_start_ms + 86400000")}`,
    );
    expect(dailyWorkflow).toContain("computeDayEnd");
    expect(dailyWorkflow).toContain('$target_date_resolved + "T00:00:00"');
    expect(dailyWorkflow).toContain("next_day");
  });

  it("keeps same-date candidate daily boundaries before re-evaluating offsets", () => {
    expect(dailyWorkflow).toContain("candidate_epoch");
    expect(dailyWorkflow).toContain('candidate_epoch | strflocaltime("%Y-%m-%d")');
    expect(dailyWorkflow).toContain("then $candidate_epoch");
    expect(dailyWorkflow).not.toContain("$e - ($b - $e)");
  });

  it("computes weekly end from the next Monday local midnight instead of a fixed 7-day add", () => {
    expect(weeklyWorkflow).not.toContain(
      `week_end_ms: ${yamlExpression("$week_start_ms + 7 * 86400000")}`,
    );
    expect(weeklyWorkflow).toContain("computeWeekEnd");
    expect(weeklyWorkflow).toContain("next_week");
  });

  it("keeps same-date candidate weekly boundaries before re-evaluating offsets", () => {
    expect(weeklyWorkflow).toContain("candidate_epoch");
    expect(weeklyWorkflow).toContain('candidate_epoch | strflocaltime("%Y-%m-%d")');
    expect(weeklyWorkflow).toContain("then $candidate_epoch");
    expect(weeklyWorkflow).not.toContain("$e - ($b - $e)");
  });

  it("guards candidate offsets in monthly and periodic import workflows too", () => {
    for (const yaml of [monthlyWorkflow, monthlyBatchWorkflow, periodicWorkflow]) {
      expect(yaml).toContain("candidate_epoch");
      expect(yaml).not.toContain("$e - ($b - $e)");
    }
    expect(monthlyWorkflow).toContain('candidate_epoch | strflocaltime("%Y-%m-%d")');
    expect(monthlyBatchWorkflow).toContain('candidate_epoch | strflocaltime("%Y-%m-%d")');
    expect(periodicWorkflow).toContain("candidate_epoch | strflocaltime");
  });

  it("retains failed path identities in every overall rollup", () => {
    for (const yaml of [dailyWorkflow, weeklyWorkflow, monthlyWorkflow]) {
      expect(yaml).toContain("v2_failed_path_buckets");
      expect(yaml).toContain("failed_path_bucket_digests");
      expect(yaml).toContain("fallback_path_bucket_digests");
      expect(yaml).toContain("missing_path_bucket_digests");
      expect(yaml).toContain("path_bucket_diagnostics");
      expect(yaml).toContain('statuses: (["failed"]');
    }
    for (const yaml of [dailyBatchWorkflow, weeklyBatchWorkflow, monthlyBatchWorkflow]) {
      expect(yaml).toContain("derivePathBucketDigest");
      expect(yaml).toContain("v2_path_bucket_digest");
      expect(yaml).toContain("v2_failed_path_buckets");
      expect(yaml).toContain("failed_path_buckets");
    }
  });

  it("uses the lower of the configured and token-counter context windows", () => {
    for (const yaml of [dailyWorkflow, weeklyWorkflow, monthlyWorkflow]) {
      expect(yaml).toContain("effective_context_window_tokens");
      expect(yaml).toContain("contextWindowTokens");
      expect(yaml).toContain("| min }");
      expect(yaml).toContain("> $effective_context_window_tokens");
    }
  });

  it("makes output guidance available to both exact-count and completion Liquid prompts", () => {
    for (const [yaml, fitStep] of [
      [dailyWorkflow, "- fitDailyInputToExactBudget:"],
      [weeklyWorkflow, "- fitWeeklyInputToExactBudget:"],
      [monthlyWorkflow, "- fitMonthlyInputToExactBudget:"],
    ] as const) {
      const initialize = yaml.indexOf("- deriveSummaryTokenBudget:");
      const fit = yaml.indexOf(fitStep);

      expect(initialize).toBeGreaterThanOrEqual(0);
      expect(fit).toBeGreaterThan(initialize);
      expect(yaml.slice(initialize, fit)).toContain(
        `summary_output_guidance: ${yamlExpression("$workflow.input.summary_output_guidance")}`,
      );
    }
  });

  it("records a planned Path bucket whose source disappears before fan-out", () => {
    for (const yaml of [dailyBatchWorkflow, weeklyBatchWorkflow, monthlyBatchWorkflow]) {
      expect(yaml).toContain(`path_single_result: "${"${"} .output | fromjson }"`);
      expect(yaml).toContain("rejectEmptyPlannedPathResult");
      expect(yaml).toContain("path source memories disappeared after planning");
      expect(yaml).toContain("$path_single_result.skipped // false");
      expect(yaml).toContain("$path_single_result.source_memory_count // 0");
    }
  });

  it("keeps every Path-plan bucket expression syntactically grouped after else", () => {
    for (const yaml of [dailyWorkflow, weeklyWorkflow, monthlyWorkflow]) {
      const pathPlan = yaml.slice(
        yaml.indexOf("- buildPathPlan:"),
        yaml.indexOf("- exitAfterPathPlan:"),
      );
      expect(pathPlan).toContain("else ($labels_by_thread");
      expect(pathPlan).not.toContain("else (($labels_by_thread");
    }
  });

  it("closes the available Path identity object before combining it with missing buckets", () => {
    for (const yaml of [dailyWorkflow, weeklyWorkflow, monthlyWorkflow]) {
      const sourcePlan = yaml.slice(
        yaml.indexOf("- buildSourcePlanSnapshot:"),
        yaml.indexOf("- fingerprintSourcePlan:"),
      );
      expect(sourcePlan).toContain('else "available" end)}))');
      expect(sourcePlan).not.toContain('else "available" end}))');
      expect(sourcePlan).toContain('sub("^.*:path:"; ""))) as $available');
    }
  });

  it("retains period failures when Path planning cannot start", () => {
    for (const [yaml, failedPeriods] of [
      [dailyBatchWorkflow, "failed_dates"],
      [weeklyBatchWorkflow, "failed_weeks"],
      [monthlyBatchWorkflow, "failed_months"],
    ] as const) {
      expect(yaml).toContain("recordPathPlanFailure");
      expect(yaml).toContain("path plan failed before summary generation");
      expect(yaml).toContain(failedPeriods);
      expect(yaml).toContain("failed_path_buckets");
    }
  });

  it("does not fan out or generate an overall summary for a failed Path plan", () => {
    for (const [yaml, failedPeriods, target] of [
      [dailyBatchWorkflow, "failed_dates", "$target_date"],
      [weeklyBatchWorkflow, "failed_weeks", "$target_week"],
      [monthlyBatchWorkflow, "failed_months", "$target_month"],
    ] as const) {
      const guard = `(($${failedPeriods} | index(${target})) == null)`;
      expect(yaml.split(guard).length - 1).toBeGreaterThanOrEqual(3);
    }
  });

  it("reports a saved partial overall as completed_with_errors", () => {
    for (const yaml of [dailyBatchWorkflow, weeklyBatchWorkflow, monthlyBatchWorkflow]) {
      expect(yaml).toContain('"completed_with_errors"');
      expect(yaml).toContain("failed_path_buckets");
    }
  });

  it("keeps legacy period summaries searchable beside scoped v2 summaries", () => {
    expect(ragWorkflow).toContain("searchLegacyPeriodSummaries:");
    expect(ragWorkflow).toContain("searchThreadGroupSummaries:");
    const groupSummarySearch = ragWorkflow.slice(
      ragWorkflow.indexOf("- searchThreadGroupSummaries:"),
      ragWorkflow.indexOf("- searchPeriodSummaries:"),
    );
    expect(groupSummarySearch).toContain('externalIdPrefix: "thread-group-summary:"');
    expect(ragWorkflow).toContain("MEMORY_KIND_DERIVED_SUMMARY");
    expect(ragWorkflow).not.toContain("MEMORY_KIND_THREAD_GROUP_SUMMARY");
    expect(ragWorkflow).toContain('then "thread_group_summary"');
    expect(ragWorkflow).toContain('sub("^thread-group-summary:"; "")');
    expect(ragWorkflow).toContain('startswith("daily:v2:")');
    expect(ragWorkflow).toContain('startswith("weekly:v2:")');
    expect(ragWorkflow).toContain('startswith("monthly:v2:")');
    expect(ragWorkflow).toContain(
      "$thread_group_summary_hits + $period_summary_hits + $legacy_period_summary_hits",
    );
    expect(ragWorkflow).toContain(
      "reduce ($thread_group_summary_hits + $period_summary_hits + $legacy_period_summary_hits",
    );
    expect(ragWorkflow).toContain(".memory.id.value == $hit.memory.id.value");
    expect(ragWorkflow).toContain("(daily|weekly|monthly):v2:");
  });

  it("does not widen an ANY period-label filter with the default overall scope", () => {
    expect(ragWorkflow).toContain('or ($workflow.input.summary_label_match // "ALL") == "ANY"');
  });

  it("includes the available LLM contract in freshness, but not passthroughs", () => {
    for (const yaml of [dailyWorkflow, weeklyWorkflow, monthlyWorkflow]) {
      expect(yaml).toContain("buildModelContractSnapshot");
      expect(yaml).toContain("fingerprintModelContract");
      expect(yaml).toContain("model_contract_fingerprint");
      expect(yaml).toContain("llm_worker_name");
      expect(yaml).toContain("context_selection_version");
      expect(yaml).toContain(
        "summary_input_byte_budget: $workflow.input.summary_input_byte_budget",
      );
      expect(yaml).toContain("temperature: 0.3");
      expect(yaml).toContain("max_tokens: $summary_max_tokens");
      expect(yaml).toContain("structured_output:");
      expect(yaml).toContain("($target_memories | length) == 1 then null");
    }
  });

  it("fingerprints the expected input plan separately from available content", () => {
    for (const yaml of [dailyWorkflow, weeklyWorkflow, monthlyWorkflow]) {
      expect(yaml).toContain("buildSourcePlanSnapshot");
      expect(yaml).toContain("fingerprintSourcePlan");
      expect(yaml).toContain("source_plan_fingerprint");
      expect(yaml).toContain('then "fallback" else "available" end');
      expect(yaml).toContain('status: "missing"');
    }
  });

  it("stops before later path or period work when stop_on_error is enabled", () => {
    for (const yaml of [dailyBatchWorkflow, weeklyBatchWorkflow, monthlyBatchWorkflow]) {
      expect(yaml).toContain("abortImmediatelyAfterPathFailure");
      expect(yaml).toContain("remaining paths were not started");
      expect(yaml).toContain("no later period was started");
      expect(yaml).not.toMatch(/summarizeEachPath:\n\s+for:[\s\S]{0,180}onError: continue/);
    }
  });
});
